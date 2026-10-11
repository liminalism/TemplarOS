//! The focused global launcher. Actions describe intent; the host performs all privileged work.

use super::{
    context_menu::{ContextMenu, LAUNCHER_MENU},
    design,
    omnibox_screen::Intent,
};
use crate::launcher_model::{LauncherMode, LauncherRow, LauncherState, LauncherTarget};
use pixelkit_raster::{Bitmap, Rect};
use pixelkit_shell::KeyInput;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};
use std::{collections::HashMap, path::PathBuf};

pub const WIDTH: i32 = 720;
pub const HEIGHT: i32 = 480;
const ROW_HEIGHT: i32 = 60;

#[derive(Debug)]
pub struct LauncherData<'a> {
    pub rows: &'a [Option<LauncherRow>],
    pub icons: &'a HashMap<String, Bitmap>,
    pub notice: Option<&'a str>,
    pub loading: bool,
    pub now_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LauncherAction {
    Open {
        target: LauncherTarget,
        intent: Intent,
    },
    QueryChanged,
    SwitchMode,
    Dismiss,
    Context {
        id: &'static str,
        target: LauncherTarget,
    },
    SetAlias {
        target: LauncherTarget,
        label: String,
    },
}

#[derive(Debug, Default)]
pub struct LauncherScreen {
    state: LauncherState,
    text: TextFieldState,
    focused: bool,
    seen_generation: u64,
    scroll: ScrollState,
    visible: (usize, usize),
    ime_caret: Option<Rect>,
    menu: ContextMenu,
    menu_target: Option<LauncherTarget>,
    alias: Option<(LauncherTarget, TextFieldState)>,
    info: Option<String>,
    info_scroll: ScrollState,
    cursor: Option<(i32, i32)>,
}

impl LauncherScreen {
    pub fn state(&self) -> &LauncherState {
        &self.state
    }
    pub fn state_mut(&mut self) -> &mut LauncherState {
        &mut self.state
    }
    pub fn show(&mut self, mode: LauncherMode, scope: Option<PathBuf>, query: String) {
        self.state.show(mode, scope, query);
        self.focus_search();
    }
    pub fn focus_search(&mut self) {
        self.text.set_text(self.state.query.clone());
        self.focused = true;
        self.reset_transient();
        self.seen_generation = self.state.generation;
    }
    pub fn reset_transient(&mut self) {
        self.scroll = ScrollState::default();
        self.visible = (0, 0);
        self.menu.close();
        self.menu_target = None;
        self.alias = None;
        self.info = None;
        self.info_scroll = ScrollState::default();
        self.cursor = None;
    }
    pub fn ime_caret(&self) -> Option<Rect> {
        self.ime_caret
    }
    pub fn visible_range(&self) -> (usize, usize) {
        self.visible
    }
    pub fn scroll_offset(&self) -> i32 {
        self.scroll.offset()
    }
    pub fn selected_target(&self, rows: &[Option<LauncherRow>]) -> Option<LauncherTarget> {
        self.state
            .selected
            .clone()
            .or_else(|| self.highlighted_target(rows))
    }
    fn highlighted_target(&self, rows: &[Option<LauncherRow>]) -> Option<LauncherTarget> {
        rows.get(self.state.highlighted)?
            .as_ref()
            .map(|row| row.target.clone())
    }
    fn commit_target(
        &self,
        rows: &[Option<LauncherRow>],
        navigated: bool,
    ) -> Option<LauncherTarget> {
        self.highlighted_target(rows).filter(|target| {
            navigated
                || self
                    .state
                    .selected
                    .as_ref()
                    .is_none_or(|selected| selected == target)
        })
    }
    pub fn show_info(&mut self, info: String) {
        self.info = Some(info);
        self.info_scroll = ScrollState::default();
    }

    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        data: &LauncherData<'_>,
    ) -> Vec<LauncherAction> {
        self.ime_caret = None;
        if !self.state.open {
            return Vec::new();
        }
        if self.seen_generation != self.state.generation {
            self.focus_search();
        }
        // The host supplied these rows for this generation. A mode/query change in this
        // frame must not let the pointer commit a target from the preceding results.
        let rows_generation = self.state.generation;
        let mut actions = Vec::new();
        design::shell(
            ui,
            area,
            design::shell_fill(),
            design::edge_strong(),
            design::Elevation::Rest,
        );
        let inner = area.inset(ui.px(14));
        let (title, remaining) = inner.split_top(ui.px(26));
        ui.label_styled(
            title,
            match self.state.mode {
                LauncherMode::Apps => "Launcher — Applications",
                LauncherMode::Files => "Launcher — Files",
            },
            design::title_style(ui),
            design::text(),
            Align::Left,
        );
        let (search, remaining) = remaining.split_top(ui.px(34));
        let (tabs, remaining) = remaining.split_top(ui.px(28));
        let (footer, body) = remaining.split_bottom(ui.px(32).min(remaining.h));
        let modal = self.alias.is_some() || self.info.is_some();
        let menu_open = self.menu.is_open();
        if let Some(chosen) = self.menu.update(ui, area)
            && let Some(target) = self.menu_target.take()
        {
            if chosen.action == "launcher.rename-alias" {
                let label = data
                    .rows
                    .iter()
                    .flatten()
                    .find(|row| row.target == target)
                    .map_or("", |row| row.label.as_str());
                self.alias = Some((target, TextFieldState::with_text(label)));
            } else {
                actions.push(LauncherAction::Context {
                    id: chosen.action,
                    target,
                });
            }
        }
        if !modal && !menu_open {
            if let Some(target) = self.state.selected.as_ref()
                && let Some(index) = data
                    .rows
                    .iter()
                    .position(|row| row.as_ref().is_some_and(|row| &row.target == target))
            {
                self.state.highlighted = index;
            }
            self.state.highlighted = self
                .state
                .highlighted
                .min(data.rows.len().saturating_sub(1));
            let keys = ui.input.take_keys();
            let capacity = (body.h / ui.px(ROW_HEIGHT).max(1)).max(1) as usize;
            let mut navigated = false;
            for key in keys {
                // Mapping a Wayland popup can replay several keys in one frame. Flush the
                // preceding text before each navigation key, so `fire`, Tab keeps `fire`
                // and two Tabs toggle twice rather than dropping the entire batch.
                if matches!(
                    key,
                    KeyInput::Escape
                        | KeyInput::Tab
                        | KeyInput::Down
                        | KeyInput::Up
                        | KeyInput::PageDown
                        | KeyInput::PageUp
                        | KeyInput::Enter
                        | KeyInput::Right
                ) || key == KeyInput::Delete && !self.focused
                {
                    self.consume_query_input(ui, search, &mut actions);
                }
                match key {
                    KeyInput::Escape => {
                        actions.clear();
                        actions.push(LauncherAction::Dismiss);
                        break;
                    }
                    KeyInput::Tab => {
                        self.state.switch_mode();
                        self.focus_search();
                        navigated = false;
                        if !actions.contains(&LauncherAction::SwitchMode) {
                            actions.push(LauncherAction::SwitchMode);
                        }
                    }
                    KeyInput::Down | KeyInput::Up | KeyInput::PageDown | KeyInput::PageUp
                        if self.state.generation == rows_generation =>
                    {
                        navigated = true;
                        self.focused = false;
                        self.state.highlighted = match key {
                            KeyInput::Down => {
                                (self.state.highlighted + 1).min(data.rows.len().saturating_sub(1))
                            }
                            KeyInput::Up => self.state.highlighted.saturating_sub(1),
                            KeyInput::PageDown => (self.state.highlighted + capacity)
                                .min(data.rows.len().saturating_sub(1)),
                            _ => self.state.highlighted.saturating_sub(capacity),
                        };
                    }
                    KeyInput::Enter | KeyInput::Right
                        if self.state.generation == rows_generation =>
                    {
                        if let Some(target) = self.commit_target(data.rows, navigated) {
                            let intent = if key == KeyInput::Right {
                                Intent::ShowAll
                            } else if ui.input.modifiers().control
                                && matches!(
                                    target,
                                    LauncherTarget::App(_) | LauncherTarget::Favorite(_)
                                )
                            {
                                Intent::LaunchNew
                            } else {
                                Intent::Open
                            };
                            if key != KeyInput::Right
                                || matches!(
                                    target,
                                    LauncherTarget::App(_) | LauncherTarget::Favorite(_)
                                )
                            {
                                actions.push(LauncherAction::Open { target, intent });
                                // One activation closes the popup. Replayed Enter repeats or
                                // a pointer press in the same frame cannot launch it twice.
                                return actions;
                            }
                        }
                    }
                    KeyInput::Delete
                        if !self.focused && self.state.generation == rows_generation =>
                    {
                        if let Some(target) = self.commit_target(data.rows, navigated) {
                            actions.push(LauncherAction::Context {
                                id: "launcher.hide",
                                target,
                            });
                            // The host must apply the filter before any later input can
                            // activate what occupied this row before it was hidden.
                            return actions;
                        }
                    }
                    KeyInput::Down
                    | KeyInput::Up
                    | KeyInput::PageDown
                    | KeyInput::PageUp
                    | KeyInput::Enter
                    | KeyInput::Right => {}
                    other => {
                        if matches!(other, KeyInput::Character(_) | KeyInput::Backspace) {
                            self.focused = true;
                        }
                        ui.input.key(other);
                    }
                }
            }
            if actions.contains(&LauncherAction::Dismiss) {
                return actions;
            }
            if navigated {
                self.state.selected = self.highlighted_target(data.rows);
                self.scroll.reveal(
                    self.state.highlighted as i32 * ui.px(ROW_HEIGHT),
                    ui.px(ROW_HEIGHT),
                    body.h,
                    data.rows.len() as i32 * ui.px(ROW_HEIGHT),
                );
            }
            if ui.input.take_click(search) {
                self.focused = true;
            }
            self.consume_query_input(ui, search, &mut actions);
        }
        let placeholder = match self.state.mode {
            LauncherMode::Apps => "Search applications…",
            LauncherMode::Files => "Search indexed files…",
        };
        ui.paint_text_field(
            &mut self.text,
            search,
            placeholder,
            self.focused && !modal && !menu_open,
            false,
        );
        if self.text.is_empty() && self.focused && !modal && !menu_open {
            let mut hint = search.inset(ui.theme.padding / 2);
            hint.x += ui.px(4);
            hint.w = (hint.w - ui.px(4)).max(0);
            ui.label(hint, placeholder, design::text_faint(), Align::Left);
        }
        self.ime_caret = self.text.caret_area();
        let (apps, files) = tabs.split_left(tabs.w / 2);
        for (rect, mode, label) in [
            (apps, LauncherMode::Apps, "Apps"),
            (files, LauncherMode::Files, "Files · Tab to switch"),
        ] {
            ui.semantic(
                pixelkit_ui::semantics::Role::Button,
                label,
                rect,
                false,
                Some(self.state.mode == mode),
                None,
            );
            ui.label_styled(
                rect,
                label,
                design::state_style(ui),
                if self.state.mode == mode {
                    design::accent()
                } else {
                    design::text_faint()
                },
                Align::Left,
            );
            if !modal && !menu_open && self.state.mode != mode && ui.input.take_click(rect) {
                self.state.switch_mode();
                self.focus_search();
                actions.push(LauncherAction::SwitchMode);
            }
        }
        let rows = if self.state.generation == rows_generation {
            data.rows
        } else {
            &[]
        };
        let row_height = ui.px(ROW_HEIGHT).max(1);
        let content = rows.len() as i32 * row_height;
        self.scroll.clamp_to(body.h, content);
        if !modal && !menu_open {
            let (_, dy) = ui.input.take_scroll(body);
            self.scroll.scroll_by(-dy.round() as i32, body.h, content);
        }
        let first = (self.scroll.offset() / row_height) as usize;
        let end = ((self.scroll.offset() + body.h).max(0) / row_height + 1) as usize;
        self.visible = (first.min(rows.len()), end.min(rows.len()));
        let cursor = ui.input.cursor();
        let pointer_moved = cursor != self.cursor;
        self.cursor = cursor;
        ui.painter.push_clip(body);
        for index in self.visible.0..self.visible.1 {
            let rect = Rect::new(
                body.x,
                body.y + index as i32 * row_height - self.scroll.offset(),
                body.w,
                row_height,
            );
            let Some(row) = &rows[index] else {
                ui.label_styled(
                    rect.inset(ui.px(10)),
                    "Loading…",
                    design::state_style(ui),
                    design::text_faint(),
                    Align::Left,
                );
                continue;
            };
            if !modal && !menu_open {
                if pointer_moved && ui.input.hovering(rect) {
                    self.state.highlighted = index;
                    self.state.selected = Some(row.target.clone());
                }
                if ui.input.take_context_click(rect) {
                    self.state.highlighted = index;
                    self.state.selected = Some(row.target.clone());
                    self.menu_target = Some(row.target.clone());
                    self.menu.open_rows_at(
                        cursor.unwrap_or((rect.x, rect.y)),
                        LAUNCHER_MENU
                            .iter()
                            .filter(|item| {
                                item.action != "launcher.pin-rail"
                                    || matches!(
                                        row.target,
                                        LauncherTarget::App(_) | LauncherTarget::Favorite(_)
                                    )
                            })
                            .collect(),
                    );
                } else if ui.input.take_click(rect) {
                    self.state.highlighted = index;
                    self.state.selected = Some(row.target.clone());
                    actions.push(LauncherAction::Open {
                        target: row.target.clone(),
                        intent: Intent::Open,
                    });
                }
            }
            let mut row_rect = rect;
            if let Some(section) = row.section
                && (index == 0
                    || rows[index - 1]
                        .as_ref()
                        .is_none_or(|previous| previous.section != row.section))
            {
                let (heading, rest) = rect.split_top(ui.px(16));
                row_rect = rest;
                ui.label_styled(
                    heading,
                    section,
                    design::state_style(ui),
                    design::text_faint(),
                    Align::Left,
                );
            }
            super::omnibox_screen::paint_launcher_row(
                ui,
                row_rect,
                row,
                index == self.state.highlighted
                    && self
                        .state
                        .selected
                        .as_ref()
                        .is_none_or(|selected| selected == &row.target),
                data.icons,
            );
        }
        ui.painter.pop_clip();
        if rows.is_empty() {
            ui.label_styled(
                body.inset(ui.px(10)),
                if data.loading {
                    "Searching…"
                } else if self.state.query.trim().is_empty() {
                    "Type to find applications or files."
                } else {
                    "No matches."
                },
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            );
        }
        ui.label_styled(
            footer,
            data.notice.unwrap_or(match self.state.mode {
                LauncherMode::Apps => "Enter: open · Ctrl+Enter: new instance · Esc: close",
                LauncherMode::Files => "Enter: open · Esc: close",
            }),
            design::state_style(ui),
            design::text_faint(),
            Align::Left,
        );
        self.menu.paint(ui, area);
        self.draw_dialog(ui, area, &mut actions);
        actions
    }

    fn consume_query_input(
        &mut self,
        ui: &mut Ui<'_>,
        search: Rect,
        actions: &mut Vec<LauncherAction>,
    ) {
        let (_, focused) = ui.text_field_input(&mut self.text, search, self.focused, false);
        self.focused = focused;
        if self.state.set_query(self.text.text().to_owned()) {
            self.seen_generation = self.state.generation;
            self.scroll = ScrollState::default();
            actions.retain(|action| {
                !matches!(
                    action,
                    LauncherAction::Open { .. } | LauncherAction::Context { .. }
                )
            });
            if !actions.contains(&LauncherAction::QueryChanged) {
                actions.push(LauncherAction::QueryChanged);
            }
        }
    }

    fn draw_dialog(&mut self, ui: &mut Ui<'_>, area: Rect, actions: &mut Vec<LauncherAction>) {
        if self.alias.is_none() && self.info.is_none() {
            return;
        }
        ui.modal_semantics();
        let width = ui.px(520).min(area.w);
        let height = ui
            .px(if self.info.is_some() { 320 } else { 140 })
            .min(area.h);
        let panel = Rect::new(
            area.x + (area.w - width) / 2,
            area.y + (area.h - height) / 2,
            width,
            height,
        );
        design::shell(
            ui,
            panel,
            design::shell_fill(),
            design::edge_strong(),
            design::Elevation::Rest,
        );
        let inner = panel.inset(ui.px(14));
        let keys = ui.input.keys().to_vec();
        let cancel = keys.contains(&KeyInput::Escape);
        if let Some(info) = &self.info {
            let (title, rest) = inner.split_top(ui.px(26));
            ui.label_styled(
                title,
                "Get info",
                design::title_style(ui),
                design::text(),
                Align::Left,
            );
            let (close, body) = rest.split_bottom(ui.px(32).min(rest.h));
            let style = design::state_style(ui);
            let text_width = (body.w - ui.px(pixelkit_ui::list::GUTTER)).max(1);
            let mut lines = Vec::new();
            for line in info.lines() {
                let ranges = ui.text.wrap(line, text_width, style);
                lines.extend(ranges.iter().map(|range| &line[range.clone()]));
            }
            let line_height = ui.px(24).max(1);
            let content = line_height.saturating_mul(lines.len() as i32);
            for key in &keys {
                let delta = match key {
                    KeyInput::Down => line_height,
                    KeyInput::Up => -line_height,
                    KeyInput::PageDown => body.h,
                    KeyInput::PageUp => -body.h,
                    _ => 0,
                };
                if delta != 0 {
                    self.info_scroll.scroll_by(delta, body.h, content);
                }
            }
            let heights = vec![line_height; lines.len()];
            ui.scroll_list(&mut self.info_scroll, body, &heights, |ui, index, rect| {
                ui.label_styled(rect, lines[index], style, design::text(), Align::Left);
            });
            if ui.button(close, "Close") || cancel || keys.contains(&KeyInput::Enter) {
                self.info = None;
            }
            let _ = ui.input.take_keys();
        } else if let Some((target, field)) = &mut self.alias {
            let (title, rest) = inner.split_top(ui.px(26));
            ui.label_styled(
                title,
                "Shortcut display name",
                design::title_style(ui),
                design::text(),
                Align::Left,
            );
            let (input, rest) = rest.split_top(ui.px(34));
            let (save, cancel_button) = rest.split_left(rest.w / 2);
            let committed = keys.contains(&KeyInput::Enter) || ui.input.take_click(save);
            let cancelled = cancel || ui.input.take_click(cancel_button);
            for (rect, label) in [
                (save, "Save shortcut name"),
                (cancel_button, "Cancel rename"),
            ] {
                ui.semantic(
                    pixelkit_ui::semantics::Role::Button,
                    label,
                    rect,
                    false,
                    None,
                    None,
                );
            }
            ui.label_styled(
                save,
                "Save",
                design::state_style(ui),
                design::accent(),
                Align::Left,
            );
            ui.label_styled(
                cancel_button,
                "Cancel",
                design::state_style(ui),
                design::text_dim(),
                Align::Left,
            );
            ui.text_field_input(field, input, true, false);
            if committed && !cancelled {
                actions.push(LauncherAction::SetAlias {
                    target: target.clone(),
                    label: field.text().to_owned(),
                });
            }
            ui.paint_text_field(field, input, "Display name", true, false);
            self.ime_caret = field.caret_area();
            if committed || cancelled {
                self.alias = None;
            }
        }
        let _ = ui.input.take_click(area);
        let _ = ui.input.take_context_click(area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, Modifiers, MouseButton, Scale};

    struct Harness {
        screen: LauncherScreen,
        rows: Vec<Option<LauncherRow>>,
        input: Input,
        text: pixelkit_text::TextCache,
        kernel: RasterKernel,
        buffer: WindowBuffer,
    }

    fn app(id: &str) -> LauncherRow {
        LauncherRow {
            target: LauncherTarget::App(id.parse().unwrap()),
            label: id.to_owned(),
            detail: "Application".into(),
            icon_id: None,
            running: false,
            section: None,
            size_bytes: None,
            mtime_ms: None,
        }
    }

    impl Harness {
        fn new(rows: Vec<Option<LauncherRow>>) -> Self {
            let mut screen = LauncherScreen::default();
            screen.show(LauncherMode::Apps, None, String::new());
            Self {
                screen,
                rows,
                input: Input::new(),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(WIDTH as u32, HEIGHT as u32),
            }
        }
        fn frame(&mut self) -> Vec<LauncherAction> {
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let icons = HashMap::new();
            let actions = self.screen.draw(
                &mut ui,
                Rect::new(0, 0, WIDTH, HEIGHT),
                &LauncherData {
                    rows: &self.rows,
                    icons: &icons,
                    notice: None,
                    loading: false,
                    now_ms: 0,
                },
            );
            self.input.end_frame();
            actions
        }
        fn key(&mut self, key: KeyInput) -> Vec<LauncherAction> {
            self.input.key(key);
            self.frame()
        }
        fn click(&mut self, index: usize, button: MouseButton) -> Vec<LauncherAction> {
            self.input
                .cursor_moved(200.0, (132 + index as i32 * ROW_HEIGHT) as f32);
            self.input.mouse(button, true);
            self.input.mouse(button, false);
            self.frame()
        }
    }

    #[test]
    fn first_click_opens_exact_target_and_never_prepares_a_shortcut() {
        let target = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(target.clone())]);
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::Open {
                target: target.target,
                intent: Intent::Open
            }]
        );
        assert!(h.frame().is_empty(), "one click cannot activate twice");
    }

    #[test]
    fn repeated_enter_and_a_simultaneous_pointer_press_activate_only_once() {
        let row = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(row.clone())]);
        h.input.key(KeyInput::Enter);
        h.input.key(KeyInput::Enter);
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::Open {
                target: row.target,
                intent: Intent::Open,
            }]
        );
    }

    #[test]
    fn hide_must_apply_before_later_keys_or_pointer_can_activate_the_old_row() {
        let row = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(row.clone())]);
        h.key(KeyInput::Down);
        h.input.key(KeyInput::Delete);
        h.input.key(KeyInput::Enter);
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::Context {
                id: "launcher.hide",
                target: row.target,
            }]
        );
    }

    #[test]
    fn enter_prefers_running_and_control_enter_explicitly_requests_an_instance() {
        let row = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(row.clone())]);
        assert_eq!(
            h.key(KeyInput::Enter),
            vec![LauncherAction::Open {
                target: row.target.clone(),
                intent: Intent::Open,
            }]
        );
        h.input.modifiers_changed(Modifiers {
            control: true,
            ..Default::default()
        });
        assert_eq!(
            h.key(KeyInput::Enter),
            vec![LauncherAction::Open {
                target: row.target,
                intent: Intent::LaunchNew,
            }]
        );
    }

    #[test]
    fn tab_retains_query_switches_modes_and_cannot_open_the_task_overview() {
        let mut h = Harness::new(vec![Some(app("org.example.Editor"))]);
        h.screen.show(LauncherMode::Apps, None, "fire".into());
        assert_eq!(h.key(KeyInput::Tab), vec![LauncherAction::SwitchMode]);
        assert_eq!(h.screen.state.mode, LauncherMode::Files);
        assert_eq!(h.screen.state.query, "fire");
        assert_eq!(h.key(KeyInput::Tab), vec![LauncherAction::SwitchMode]);
        assert_eq!(h.screen.state.mode, LauncherMode::Apps);
        assert_eq!(h.screen.state.query, "fire");
        assert_eq!(h.key(KeyInput::Escape), vec![LauncherAction::Dismiss]);
    }

    #[test]
    fn immediate_text_and_tab_replayed_together_keep_text_in_the_new_mode() {
        let mut h = Harness::new(vec![Some(app("org.example.Editor"))]);
        for character in "fire".chars() {
            h.input.key(KeyInput::Character(character));
        }
        h.input.key(KeyInput::Tab);
        assert_eq!(
            h.frame(),
            vec![LauncherAction::QueryChanged, LauncherAction::SwitchMode]
        );
        assert_eq!(h.screen.state.mode, LauncherMode::Files);
        assert_eq!(h.screen.state.query, "fire");

        // A complete replay batch can contain several switches and later text edits.
        h.input.key(KeyInput::Tab);
        h.input.key(KeyInput::Character('s'));
        h.input.key(KeyInput::Tab);
        let actions = h.frame();
        assert_eq!(
            actions,
            vec![LauncherAction::SwitchMode, LauncherAction::QueryChanged]
        );
        assert_eq!(h.screen.state.mode, LauncherMode::Files);
        assert_eq!(h.screen.state.query, "fires");
    }

    #[test]
    fn escape_in_a_replayed_batch_cannot_activate_a_simultaneous_pointer_result() {
        let mut h = Harness::new(vec![Some(app("org.example.Editor"))]);
        h.input.key(KeyInput::Escape);
        assert_eq!(h.click(0, MouseButton::Left), vec![LauncherAction::Dismiss]);
    }

    #[test]
    fn typing_or_switching_cannot_click_results_from_the_preceding_generation() {
        let mut h = Harness::new(vec![Some(app("org.example.Editor"))]);
        h.input.key(KeyInput::Character('f'));
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::QueryChanged]
        );
        assert_eq!(h.screen.state.query, "f");
        h.input.key(KeyInput::Tab);
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::SwitchMode]
        );
    }

    #[test]
    fn a_context_menu_keeps_its_target_when_async_results_change_under_it() {
        let first = app("org.example.First");
        let second = app("org.example.Second");
        let mut h = Harness::new(vec![Some(first.clone()), Some(second)]);
        assert!(h.click(0, MouseButton::Right).is_empty());
        assert!(h.screen.menu.is_open());
        h.rows.swap(0, 1);
        // The first Down selects the first item from the neutral menu state.
        for _ in 0..4 {
            h.input.key(KeyInput::Down);
        }
        h.input.key(KeyInput::Enter);
        assert_eq!(
            h.frame(),
            vec![LauncherAction::Context {
                id: "launcher.info",
                target: first.target,
            }]
        );
    }

    #[test]
    fn alias_commit_is_explicit_metadata_and_escape_cancels_it() {
        let row = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(row.clone())]);
        assert!(h.click(0, MouseButton::Right).is_empty());
        h.input.key(KeyInput::Down);
        h.input.key(KeyInput::Down);
        h.input.key(KeyInput::Down);
        h.input.key(KeyInput::Enter);
        assert!(h.frame().is_empty());
        assert!(h.screen.alias.is_some());
        assert!(h.key(KeyInput::Escape).is_empty());
        assert!(h.screen.alias.is_none());
        h.screen.alias = Some((row.target.clone(), TextFieldState::with_text("Notes")));
        assert_eq!(
            h.key(KeyInput::Enter),
            vec![LauncherAction::SetAlias {
                target: row.target,
                label: "Notes".into(),
            }]
        );
    }

    #[test]
    fn long_information_wraps_scrolls_and_closes_without_activating_a_result() {
        let mut h = Harness::new(vec![Some(app("org.example.Editor"))]);
        h.screen.show_info(format!(
            "Native path\nfile:///work/{}\nArguments: literal values",
            "a-long-directory-name/".repeat(80)
        ));
        assert!(h.frame().is_empty());
        assert!(h.key(KeyInput::PageDown).is_empty());
        assert!(h.screen.info_scroll.offset() > 0);
        assert!(h.key(KeyInput::Escape).is_empty());
        assert!(h.screen.info.is_none());
        assert!(h.screen.state.open);
        h.screen.show_info("Another application".into());
        assert_eq!(h.screen.info_scroll.offset(), 0);
    }

    #[test]
    fn scrolling_survives_redraw_and_keyboard_demand_includes_unloaded_slots() {
        let rows = (0..40)
            .map(|index| Some(app(&format!("org.example.App{index}"))))
            .collect();
        let mut h = Harness::new(rows);
        h.frame();
        h.input.cursor_moved(200.0, 300.0);
        h.input.scrolled(0.0, -300.0);
        h.frame();
        let offset = h.screen.scroll_offset();
        assert!(offset > 0);
        h.frame();
        assert_eq!(h.screen.scroll_offset(), offset);
        h.rows = vec![None; 40];
        h.screen.state.selected = None;
        for _ in 0..3 {
            h.input.key(KeyInput::PageDown);
        }
        h.frame();
        let (first, end) = h.screen.visible_range();
        assert!(first > 0 && end > first);
        assert!(h.screen.state.highlighted >= first && h.screen.state.highlighted < end);
        assert!(
            h.key(KeyInput::Enter).is_empty(),
            "an unloaded path cannot open"
        );
    }

    #[test]
    fn delete_on_a_navigated_result_only_requests_hide() {
        let row = app("org.example.Editor");
        let mut h = Harness::new(vec![Some(row.clone())]);
        h.key(KeyInput::Down);
        assert_eq!(
            h.key(KeyInput::Delete),
            vec![LauncherAction::Context {
                id: "launcher.hide",
                target: row.target,
            }]
        );
        h.key(KeyInput::Character('e'));
        assert_eq!(h.screen.state.query, "e", "typing returns to the query");
    }

    #[test]
    fn native_file_activation_preserves_bytes_and_has_no_new_instance_intent() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let target = LauncherTarget::File(PathBuf::from(OsString::from_vec(
            b"/tmp/report-\xff".to_vec(),
        )));
        let row = LauncherRow {
            target: target.clone(),
            ..app("org.example.Editor")
        };
        let mut h = Harness::new(vec![Some(row)]);
        h.screen.show(LauncherMode::Files, None, "report".into());
        assert_eq!(
            h.click(0, MouseButton::Left),
            vec![LauncherAction::Open {
                target: target.clone(),
                intent: Intent::Open
            }]
        );
        h.input.modifiers_changed(Modifiers {
            control: true,
            ..Default::default()
        });
        assert_eq!(
            h.key(KeyInput::Enter),
            vec![LauncherAction::Open {
                target,
                intent: Intent::Open
            }]
        );
        assert!(h.key(KeyInput::Right).is_empty());
    }

    #[test]
    fn snapshot_refresh_retains_the_selected_identity_without_opening_a_replacement() {
        let first = app("org.example.First");
        let second = app("org.example.Second");
        let mut h = Harness::new(vec![Some(first.clone()), Some(second.clone())]);
        h.key(KeyInput::Down);
        assert_eq!(
            h.screen.selected_target(&h.rows),
            Some(second.target.clone())
        );
        // The replacement cursor has not fetched the selected row yet. Its former
        // numeric slot is a different object, and Enter must never open that object.
        h.rows = vec![None, Some(first)];
        assert!(h.key(KeyInput::Enter).is_empty());
        assert_eq!(
            h.screen.selected_target(&h.rows),
            Some(second.target.clone())
        );
        h.rows = vec![Some(second.clone()), None];
        assert_eq!(
            h.key(KeyInput::Enter),
            vec![LauncherAction::Open {
                target: second.target,
                intent: Intent::Open,
            }]
        );
        assert_eq!(h.screen.state.highlighted, 0);
    }
}
