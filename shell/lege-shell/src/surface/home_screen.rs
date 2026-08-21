//! The Home Surface screen: drawer navigation, a search field, and the drawer's content — the
//! graphical front end over `crate::drawers`/`crate::data`'s already-tested models.
//!
//! Nine of the ten drawers (everything but the Window Shelf) show the exact lines
//! `lege-shell --print <drawer>` prints: this crate's business logic already turns a live D-Bus
//! answer into readable, column-aligned text, and re-deriving a bespoke table layout per drawer
//! shape (ten different ones) would be new surface, new bugs and no more honest than drawing the
//! same tested lines in a scrolling panel with real navigation, a real search field and a real
//! scrollbar around them. The Window Shelf is the one drawer this screen renders structurally
//! rather than as text, because it is the one the plan requires an action on ("can restore one",
//! `docs/planning/01-initial-shape.md` §1) — a line of text has no button.

use std::collections::HashMap;

use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};

use lege_window::{ShellToplevel, WindowId};

use crate::drawers::window_shelf;
use crate::home_surface::Drawer;

/// The three drawers `Files.Search1` can search — see [`Drawer`]'s doc: each is a standard XDG
/// path plus its saved queries, which is exactly what a query root should be.
const SEARCHABLE: [Drawer; 3] = [Drawer::Downloads, Drawer::Documents, Drawer::Media];

/// What the screen asks its caller to do — this struct never touches D-Bus or the Wayland
/// connection itself, so its logic is testable with a plain [`WindowBuffer`](pixelkit_raster::WindowBuffer)
/// and no service of any kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeSurfaceAction {
    None,
    /// Enter was pressed in the search field while a searchable drawer was selected.
    RunSearch {
        drawer: Drawer,
        query: String,
    },
    /// The Window Shelf's Restore button was clicked for this window.
    RestoreWindow(WindowId),
}

/// Everything the screen reads but does not own — refreshed by [`crate::surface::app`] from
/// `crate::data`'s background fetch and the live `lege_shell_v1` connection.
#[derive(Debug)]
pub struct HomeSurfaceData<'a> {
    /// Every D-Bus-backed drawer's already-rendered text, split into lines. Absent for a drawer
    /// whose first fetch has not landed yet.
    pub drawer_texts: &'a HashMap<Drawer, Vec<String>>,
    /// Lines from the last `Files.Search1` search, if the selected drawer has one active.
    pub search_results: Option<&'a [String]>,
    /// Every window the live connection knows about, for the Window Shelf.
    pub toplevels: &'a [ShellToplevel],
    /// Whether the trusted-shell connection is up at all — the Window Shelf explains itself
    /// differently when it is not (see [`HomeSurfaceScreen::draw`]'s empty-state text).
    pub connected: bool,
}

#[derive(Debug)]
pub struct HomeSurfaceScreen {
    pub selected: Drawer,
    pub search: TextFieldState,
    pub search_focused: bool,
    pub scroll: ScrollState,
}

impl Default for HomeSurfaceScreen {
    fn default() -> HomeSurfaceScreen {
        HomeSurfaceScreen {
            selected: Drawer::Recent,
            search: TextFieldState::new(),
            search_focused: false,
            scroll: ScrollState::new(),
        }
    }
}

const RAIL_WIDTH: i32 = 160;
const SEARCH_HEIGHT: i32 = 36;
const ROW_HEIGHT: i32 = 26;

impl HomeSurfaceScreen {
    /// Draw one frame at `area` (the window's full content rect) and report what the user asked
    /// for. Selecting a different drawer, or typing in the search field, is handled internally —
    /// only actions the caller must act on outside this struct's own state come back out.
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        data: &HomeSurfaceData<'_>,
    ) -> HomeSurfaceAction {
        ui.painter.fill_rect(area, ui.theme.background);

        let (rail, rest) = area.split_left(ui.px(RAIL_WIDTH));
        self.draw_rail(ui, rail);

        let rest = rest.inset(ui.px(8));
        let (search_area, content_area) = rest.split_top(ui.px(SEARCH_HEIGHT));

        // Read before `text_field` consumes the frame's keys — see the module-external note in
        // `text_field`'s own doc: a focused field takes every key, including ones (like Enter)
        // its own `TextFieldState::apply` does nothing with, so Enter must be checked first.
        let enter_pressed = self.search_focused && ui.input.keys().contains(&KeyInput::Enter);
        if ui.input.take_click(search_area) {
            self.search_focused = true;
        }
        let placeholder = if SEARCHABLE.contains(&self.selected) {
            "Search this drawer (Enter to search)"
        } else {
            "Search (only Downloads, Documents and Media are searchable)"
        };
        ui.text_field(
            &mut self.search,
            search_area.inset(ui.px(4)),
            placeholder,
            self.search_focused,
            false,
        );

        if self.selected == Drawer::WindowShelf {
            self.draw_window_shelf(ui, content_area, data)
        } else {
            self.draw_text_drawer(ui, content_area, data);
            if enter_pressed
                && SEARCHABLE.contains(&self.selected)
                && !self.search.text().is_empty()
            {
                HomeSurfaceAction::RunSearch {
                    drawer: self.selected,
                    query: self.search.text().to_owned(),
                }
            } else {
                HomeSurfaceAction::None
            }
        }
    }

    fn draw_rail(&mut self, ui: &mut Ui<'_>, rail: Rect) {
        ui.painter.fill_rect(rail, ui.theme.panel);
        let row_height = ui.px(ROW_HEIGHT + 6);
        for (index, drawer) in Drawer::DEFAULT.into_iter().enumerate() {
            let row = Rect::new(
                rail.x,
                rail.y + index as i32 * row_height,
                rail.w,
                row_height,
            );
            let active = drawer == self.selected;
            let fill = if active {
                ui.theme.selection
            } else if ui.input.hovering(row) {
                ui.theme.hover
            } else {
                ui.theme.panel
            };
            ui.painter.fill_rect(row, fill);
            let colour = if active {
                ui.theme.text
            } else {
                ui.theme.text_dim
            };
            ui.label(row.inset(ui.px(8)), drawer.name(), colour, Align::Left);
            if ui.input.take_click(row) && !active {
                self.selected = drawer;
                self.scroll = ScrollState::new();
                self.search_focused = false;
            }
        }
    }

    /// The lines this drawer shows right now: a live search's results if one is active, else the
    /// drawer's own already-fetched text — one line per already-tested record.
    fn current_lines<'a>(&self, data: &'a HomeSurfaceData<'a>) -> &'a [String] {
        if let Some(results) = data.search_results
            && SEARCHABLE.contains(&self.selected)
        {
            return results;
        }
        data.drawer_texts
            .get(&self.selected)
            .map_or(&[] as &[String], Vec::as_slice)
    }

    fn draw_text_drawer(&mut self, ui: &mut Ui<'_>, area: Rect, data: &HomeSurfaceData<'_>) {
        ui.panel(area);
        let lines = self.current_lines(data);
        if lines.is_empty() {
            ui.label(
                area.inset(ui.px(12)),
                "Loading…",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let row_height = ui.px(ROW_HEIGHT);
        let heights = vec![row_height; lines.len()];
        let inner = area.inset(ui.px(6));
        ui.scroll_list(&mut self.scroll, inner, &heights, |ui, index, rect| {
            let hovered = ui.input.hovering(rect);
            if hovered {
                ui.painter.fill_rect(rect, ui.theme.hover);
            }
            ui.label(
                rect.inset(ui.px(6)),
                &lines[index],
                ui.theme.text,
                Align::Left,
            );
        });
    }

    fn draw_window_shelf(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        data: &HomeSurfaceData<'_>,
    ) -> HomeSurfaceAction {
        ui.panel(area);
        if !data.connected {
            ui.label(
                area.inset(ui.px(12)),
                "Not connected to the compositor's lege_shell_v1 protocol — no window list.",
                ui.theme.text_dim,
                Align::Left,
            );
            return HomeSurfaceAction::None;
        }
        let shelved = window_shelf::shelved(data.toplevels);
        if shelved.is_empty() {
            ui.label(
                area.inset(ui.px(12)),
                "Windows — nothing shelved",
                ui.theme.text_dim,
                Align::Left,
            );
            return HomeSurfaceAction::None;
        }

        let row_height = ui.px(ROW_HEIGHT + 10);
        let heights = vec![row_height; shelved.len()];
        let inner = area.inset(ui.px(6));
        let mut restore = None;
        ui.scroll_list(&mut self.scroll, inner, &heights, |ui, index, rect| {
            let toplevel = shelved[index];
            if ui.input.hovering(rect) {
                ui.painter.fill_rect(rect, ui.theme.hover);
            }
            let button_width = ui.px(90);
            let (button_area, label_area) = rect.split_right(button_width);
            let title = if toplevel.title.is_empty() {
                "(untitled)"
            } else {
                &toplevel.title
            };
            let app = toplevel
                .canonical_app_id
                .as_ref()
                .map_or_else(|| "(unidentified)".to_owned(), ToString::to_string);
            ui.label(
                label_area.inset(ui.px(6)),
                &format!("{app} — {title}"),
                ui.theme.text,
                Align::Left,
            );
            if ui.button(button_area.inset(ui.px(4)), "Restore") {
                restore = Some(toplevel.window_id);
            }
        });
        restore.map_or(HomeSurfaceAction::None, HomeSurfaceAction::RestoreWindow)
    }
}
