//! The Home Field screen: the document from `crate::field_model` drawn as tiles, with a search
//! field above it, the drawers' full bodies a click away, and every action reachable by pointer
//! *and* by keyboard. Like every screen in this crate, a pure `draw(ui, area, data) -> actions`
//! over a `WindowBuffer`: no D-Bus, no Wayland, so it is tested with a synthetic `Input`.
//!
//! What this draws is decided elsewhere — `lege_field` owns where tiles are and what a drop
//! means; `crate::field_model` owns which tiles exist. This module owns only pixels and the
//! translation of pointer/keyboard gestures into `FieldAction`s. The visual grammar is
//! `docs/homefield.md`'s: flat, calm tile shells; type expressed inside the rectangle, never by
//! its silhouette; a raised shadow only while a tile is lifted; one activity border for a live
//! window; the pending operation spelled out in text before release.

use std::collections::HashMap;

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::{TextFieldState, Ui};

use lege_field::drag::{Drag, DragConfig, DragSample, GrabOffset, Release};
use lege_field::focus::Direction;
use lege_field::{
    Cell, FieldDocument, FieldOp, Landing, Operation, PixelRect, Placement, Span, SpanRange,
    Target, Tile, TileId, TileKind, apply, cell_at, tile_rect,
};
use lege_types::AppId;
use lege_window::{ShellToplevel, WindowId};

use crate::field_model::{self, GAP};
use crate::home_surface::Drawer;
use crate::surface::home_screen::{HomeSurfaceAction, HomeSurfaceData, HomeSurfaceScreen};

/// What the screen asks its caller to do. Everything that touches a service or the compositor
/// comes out here; nothing in this module performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldAction {
    RunSearch {
        drawer: Drawer,
        query: String,
    },
    Launch(AppId),
    Activate(WindowId),
    Park(WindowId),
    Restore(WindowId),
    RevealHome,
    /// A field op the user performed; the caller applies it and keeps the inverse for undo.
    Apply(FieldOp),
    Undo,
    /// A drawer tile is large enough to show its directory as a treemap and has none at this
    /// size: the caller scans and rasterizes one of `width`×`height` pixels off-thread.
    NeedTreemap {
        drawer: Drawer,
        width: u32,
        height: u32,
    },
    /// One pointer drop happened — the throw experiment's measurements
    /// (`docs/research/throw-spike.md`). Emitted before the `Apply` it led to.
    DropMetrics(DropMetrics),
}

/// What one drop measured. Logged by the app only while the experiment is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropMetrics {
    pub thrown: bool,
    /// Lift to release: the time-to-target.
    pub lifted_for_ms: u64,
    /// The drop named a semantic operation (group, add to project…) rather than placing.
    pub semantic: bool,
    /// The same tile was dropped less than three seconds ago: a retry.
    pub relift: bool,
    /// The operation's label, for the log.
    pub op: &'static str,
}

/// Everything the screen reads but does not own.
#[derive(Debug)]
pub struct FieldData<'a> {
    pub field: &'a FieldDocument,
    pub toplevels: &'a [ShellToplevel],
    /// App icons by canonical app id.
    pub icons: &'a HashMap<String, Bitmap>,
    /// Window previews by session identifier (phase 6; empty until then).
    pub previews: &'a HashMap<String, Bitmap>,
    pub drawer_texts: &'a HashMap<Drawer, Vec<String>>,
    /// Rasterized cushion treemaps of the standard-path drawers' directories
    /// (`lege_treemap`), keyed by drawer, at whatever size was last requested.
    pub treemaps: &'a HashMap<Drawer, Bitmap>,
    /// Each such drawer's composition: `(0x00RRGGBB, share)` per file category, largest first.
    pub compositions: &'a HashMap<Drawer, Vec<(u32, f32)>>,
    pub search_results: Option<&'a [String]>,
    pub connected: bool,
    pub home_revealed: bool,
    /// Monotonic milliseconds, for the drag's velocity.
    pub now_ms: u64,
}

/// The ghost shown while a tile is lifted: where it would land, and who would move.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ghost {
    placement: Placement,
    moved: Vec<(TileId, Placement)>,
    label: Option<&'static str>,
    label_at: Option<Rect>,
}

#[derive(Debug)]
pub struct FieldScreen {
    pub drag: Drag,
    pub drag_cfg: DragConfig,
    pub focus: Option<TileId>,
    /// Space on a focused tile: it is lifted by keyboard, and the arrows move this landing.
    pub keyboard_move: Option<(TileId, Cell)>,
    /// A drawer opened to its full body, over the field.
    pub expanded: Option<Drawer>,
    /// A project zoomed to fill the field: its interior grid is what is drawn and edited, with
    /// a breadcrumb back. The document is untouched by zoom — it is a view, never a hidden
    /// desktop (`docs/homefield.md`, "Do not recreate virtual desktops under another name").
    pub zoomed: Option<TileId>,
    pub drawer_view: HomeSurfaceScreen,
    pub search: TextFieldState,
    pub search_focused: bool,
    /// The field's pixel rectangle from the last frame, for tests and hit-testing.
    pub bounds: Rect,
    /// The last pointer drop, for the experiment's retry measure.
    last_drop: Option<(TileId, u64)>,
}

impl Default for FieldScreen {
    fn default() -> Self {
        Self {
            drag: Drag::Idle,
            drag_cfg: DragConfig::default(),
            focus: None,
            keyboard_move: None,
            expanded: None,
            zoomed: None,
            drawer_view: HomeSurfaceScreen::default(),
            search: TextFieldState::new(),
            search_focused: false,
            bounds: Rect::new(0, 0, 0, 0),
            last_drop: None,
        }
    }
}

const SEARCH_HEIGHT: i32 = 36;
const BAND_HEIGHT: i32 = 22;
const CRUMB_HEIGHT: i32 = 30;

impl FieldScreen {
    /// Whether the caller should keep redrawing on a timer: only while something is lifted.
    pub fn animating(&self) -> bool {
        self.drag.is_lifted()
    }

    /// The pixel rectangle of `tile` as last drawn: on the top level, its own rect (a child
    /// inside a project is drawn nested within it, `lege_field::tile_rect`); zoomed into a
    /// project, only that project's children have rects, laid out on the interior grid.
    pub fn tile_rect(&self, doc: &FieldDocument, tile: TileId, ui: &Ui<'_>) -> Option<Rect> {
        match self.zoomed {
            None => tile_rect(doc, to_pixel(self.bounds), tile, ui.px(GAP)).map(from_pixel),
            Some(project) => {
                let located = doc.location(tile)?;
                if located.container != Some(project) {
                    return None;
                }
                let (cols, rows) = doc.grid_size(Some(project))?;
                Some(from_pixel(lege_field::cell_rect(
                    cols,
                    rows,
                    to_pixel(self.bounds),
                    located.placement.cell,
                    located.placement.span,
                    ui.px(GAP),
                )))
            }
        }
    }

    /// The grid being drawn and edited: the field, or the zoomed project's interior.
    fn grid_dims(&self, doc: &FieldDocument) -> (u16, u16) {
        doc.grid_size(self.zoomed).unwrap_or((doc.cols, doc.rows))
    }

    /// Every tile on the current grid with its rect, in reading order.
    fn current_rects(&self, doc: &FieldDocument, ui: &Ui<'_>) -> Vec<(TileId, Rect)> {
        let mut rects: Vec<(TileId, Rect)> = doc
            .placements_in(self.zoomed)
            .into_iter()
            .filter_map(|(id, _)| self.tile_rect(doc, id, ui).map(|rect| (id, rect)))
            .collect();
        rects.sort_by_key(|(_, rect)| (rect.y, rect.x));
        rects
    }

    /// The grab band at the top of a tile's rect.
    pub fn band(rect: Rect, ui: &Ui<'_>) -> Rect {
        rect.split_top(ui.px(BAND_HEIGHT).min(rect.h)).0
    }

    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &FieldData<'_>) -> Vec<FieldAction> {
        ui.painter.fill_rect(area, ui.theme.background);
        let mut actions = Vec::new();

        let (bar, rest) = area.inset(ui.px(8)).split_top(ui.px(SEARCH_HEIGHT));
        self.draw_search_bar(ui, bar, data, &mut actions);

        if let Some(drawer) = self.expanded {
            self.draw_expanded(ui, rest, drawer, data, &mut actions);
            return actions;
        }

        let doc = data.field;
        if self.zoomed.is_some_and(|id| doc.container(id).is_none()) {
            self.zoomed = None;
        }
        let rest = if let Some(project) = self.zoomed {
            let (crumb, body) = rest.split_top(ui.px(CRUMB_HEIGHT));
            let name = doc
                .tile(project)
                .map_or("project", |tile| tile.title.as_str());
            let style = TextStyle::new(ui.theme.body.face, 13.0);
            ui.label_styled(
                crumb.inset(ui.px(6)),
                &format!(
                    "HOME  ▸  {}   (Escape returns; Backspace moves a tile back out)",
                    name.to_uppercase()
                ),
                style,
                ui.theme.text_dim,
                Align::Left,
            );
            ui.painter.stroke_rect(body, ui.px(2), ui.theme.accent_soft);
            body
        } else {
            rest
        };
        self.bounds = rest;
        if self
            .focus
            .is_some_and(|id| doc.location(id).is_none_or(|l| l.container != self.zoomed))
        {
            self.focus = None;
        }
        self.handle_keys(ui, doc, data, &mut actions);
        self.handle_pointer(ui, doc, data, &mut actions);

        let ghost = self.ghost(doc, ui);
        let lifted = self.drag.tile().filter(|_| self.drag.is_lifted());
        let rects = self.current_rects(doc, ui);
        for (id, rect) in &rects {
            if Some(*id) == lifted {
                continue;
            }
            let Some(tile) = doc.tile(*id) else { continue };
            let visual = TileVisual {
                hovered: ui.input.hovering(*rect),
                focused: self.focus == Some(*id),
                lifted: false,
                displaced: ghost
                    .as_ref()
                    .and_then(|g| g.moved.iter().find(|(m, _)| m == id))
                    .map(|(_, to)| to.cell),
            };
            self.paint_tile(ui, *rect, tile, visual, data, &mut actions);
        }

        self.draw_overlays(ui, doc, data, ghost.as_ref(), lifted, &mut actions);

        if rects.is_empty() {
            ui.label(
                rest.inset(ui.px(12)),
                if self.zoomed.is_some() {
                    "Empty project — drop tiles onto its tile on the field to fill it"
                } else if data.connected {
                    "Home Field — nothing here yet; open an application and it will appear"
                } else {
                    "Home Field — not connected to the compositor's lege_shell_v1 protocol"
                },
                ui.theme.text_dim,
                Align::Left,
            );
        }
        actions
    }

    /// The ghost (where a lifted tile would land, who would move, the pending operation's
    /// label) and the lifted tile itself, drawn over everything else.
    fn draw_overlays(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        ghost: Option<&Ghost>,
        lifted: Option<TileId>,
        actions: &mut Vec<FieldAction>,
    ) {
        let (cols, rows) = self.grid_dims(doc);
        if let Some(ghost) = ghost {
            let ghost_rect = from_pixel(lege_field::cell_rect(
                cols,
                rows,
                to_pixel(self.bounds),
                ghost.placement.cell,
                ghost.placement.span,
                ui.px(GAP),
            ));
            ui.painter
                .stroke_rect(ghost_rect, ui.px(2), ui.theme.accent);
            ui.painter.blend_rect(ghost_rect, ui.theme.accent, 40);
            for (id, to) in &ghost.moved {
                let Some(located) = doc.location(*id) else {
                    continue;
                };
                let preview = from_pixel(lege_field::cell_rect(
                    cols,
                    rows,
                    to_pixel(self.bounds),
                    to.cell,
                    located.placement.span,
                    ui.px(GAP),
                ));
                ui.painter
                    .stroke_rect(preview, ui.px(1), ui.theme.accent_soft);
            }
            if let (Some(label), Some(at)) = (ghost.label, ghost.label_at) {
                let style = TextStyle::new(ui.theme.body.face, 12.0);
                ui.pill(
                    at,
                    label,
                    style,
                    ui.theme.text,
                    ui.theme.selection,
                    ui.theme.accent,
                    true,
                );
            }
        }

        if let (Some(id), Some((x, y))) = (lifted, self.drag.tile_position())
            && let (Some(tile), Some(rect)) = (doc.tile(id), self.tile_rect(doc, id, ui))
        {
            let raised = Rect::new(x as i32, y as i32, rect.w, rect.h);
            let visual = TileVisual {
                hovered: false,
                focused: false,
                lifted: true,
                displaced: None,
            };
            self.paint_tile(ui, raised, tile, visual, data, actions);
        }
    }

    fn draw_search_bar(
        &mut self,
        ui: &mut Ui<'_>,
        bar: Rect,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let (home, search_area) = bar.split_left(ui.px(80));
        let home_fill = if data.home_revealed {
            ui.theme.selection
        } else if ui.input.hovering(home) {
            ui.theme.hover
        } else {
            ui.theme.panel_alt
        };
        ui.painter
            .fill_rounded_rect(home, ui.theme.corner_radius, home_fill);
        ui.label(home, "Home", ui.theme.text, Align::Centre);
        if ui.input.take_click(home) {
            if self.expanded.is_some() {
                self.expanded = None;
            } else {
                actions.push(FieldAction::RevealHome);
            }
        }
        let search_area = Rect::new(
            search_area.x + ui.px(8),
            search_area.y,
            (search_area.w - ui.px(8)).max(0),
            search_area.h,
        );
        let enter = self.search_focused && ui.input.keys().contains(&KeyInput::Enter);
        if ui.input.take_click(search_area) {
            self.search_focused = true;
        }
        let searchable = self.expanded.is_some_and(|drawer| {
            matches!(
                drawer,
                Drawer::Downloads | Drawer::Documents | Drawer::Media
            )
        });
        let placeholder = if searchable {
            "Search this drawer (Enter to search)"
        } else {
            "Search — open Downloads, Documents or Media to search it"
        };
        ui.text_field(
            &mut self.search,
            search_area.inset(ui.px(4)),
            placeholder,
            self.search_focused,
            false,
        );
        if enter
            && searchable
            && !self.search.text().is_empty()
            && let Some(drawer) = self.expanded
        {
            actions.push(FieldAction::RunSearch {
                drawer,
                query: self.search.text().to_owned(),
            });
        }
    }

    fn draw_expanded(
        &mut self,
        ui: &mut Ui<'_>,
        rest: Rect,
        drawer: Drawer,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let (crumb, body) = rest.split_top(ui.px(CRUMB_HEIGHT));
        let style = TextStyle::new(ui.theme.body.face, 13.0);
        ui.label_styled(
            crumb.inset(ui.px(6)),
            &format!(
                "HOME  ▸  {}   (Escape returns)",
                drawer.name().to_uppercase()
            ),
            style,
            ui.theme.text_dim,
            Align::Left,
        );
        if !self.search_focused && ui.input.keys().contains(&KeyInput::Escape) {
            self.expanded = None;
            return;
        }
        self.drawer_view.selected = drawer;
        let inner = HomeSurfaceData {
            drawer_texts: data.drawer_texts,
            search_results: data.search_results,
            toplevels: data.toplevels,
            connected: data.connected,
        };
        match self.drawer_view.draw(ui, body, &inner) {
            HomeSurfaceAction::RunSearch { drawer, query } => {
                actions.push(FieldAction::RunSearch { drawer, query });
            }
            HomeSurfaceAction::RestoreWindow(window_id) => {
                actions.push(FieldAction::Restore(window_id));
            }
            HomeSurfaceAction::None => {}
        }
        // The rail inside the drawer view may have switched drawers; follow it.
        self.expanded = Some(self.drawer_view.selected);
    }

    fn handle_keys(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        if self.search_focused {
            if ui.input.keys().contains(&KeyInput::Escape) {
                self.search_focused = false;
            }
            return;
        }
        let keys: Vec<KeyInput> = ui.input.keys().to_vec();
        let modifiers = ui.input.modifiers();
        let (cols, rows) = self.grid_dims(doc);
        let rects = self.current_rects(doc, ui);
        for key in keys {
            if let Some((tile, cell)) = self.keyboard_move {
                self.handle_move_key(&key, tile, cell, cols, rows, actions);
                continue;
            }
            match key {
                KeyInput::Left | KeyInput::Right | KeyInput::Up | KeyInput::Down => {
                    let direction = match key {
                        KeyInput::Left => Direction::Left,
                        KeyInput::Right => Direction::Right,
                        KeyInput::Up => Direction::Up,
                        _ => Direction::Down,
                    };
                    self.focus = match self.focus {
                        None => first_in(&rects),
                        Some(from) => neighbour_in(&rects, from, direction).or(Some(from)),
                    };
                }
                KeyInput::Tab => {
                    self.focus = self.focus.map_or_else(
                        || first_in(&rects),
                        |from| {
                            neighbour_in(&rects, from, Direction::Right)
                                .or_else(|| neighbour_in(&rects, from, Direction::Down))
                                .or(Some(from))
                        },
                    );
                }
                KeyInput::Backspace => {
                    // Inside a project: move the focused tile back out onto the field, next to
                    // the project.
                    if let Some(project) = self.zoomed
                        && let Some(id) = self.focus
                        && let Some(home) = doc.location(project)
                    {
                        actions.push(FieldAction::Apply(FieldOp::Move {
                            id,
                            landing: Landing {
                                container: None,
                                cell: home.placement.cell,
                            },
                        }));
                    }
                }
                KeyInput::Enter => {
                    if let Some(id) = self.focus
                        && let Some(tile) = doc.tile(id)
                    {
                        self.primary_action(tile, data, actions);
                    }
                }
                KeyInput::Character(' ') => {
                    if let Some(id) = self.focus
                        && let Some(located) = doc.location(id)
                        && located.container == self.zoomed
                    {
                        self.keyboard_move = Some((id, located.placement.cell));
                    }
                }
                KeyInput::Character('z' | 'Z') if modifiers.control => {
                    actions.push(FieldAction::Undo);
                }
                KeyInput::Delete => {
                    if let Some(id) = self.focus {
                        actions.push(FieldAction::Apply(FieldOp::Stash { id }));
                    }
                }
                KeyInput::Escape if self.zoomed.is_some() => self.zoomed = None,
                KeyInput::Escape => self.focus = None,
                _ => {}
            }
        }
    }

    /// Arrow keys move a keyboard-lifted tile's landing cell; Enter drops it on the current
    /// grid; Escape puts it back.
    fn handle_move_key(
        &mut self,
        key: &KeyInput,
        tile: TileId,
        cell: Cell,
        cols: u16,
        rows: u16,
        actions: &mut Vec<FieldAction>,
    ) {
        match *key {
            KeyInput::Left | KeyInput::Right | KeyInput::Up | KeyInput::Down => {
                let cell = match key {
                    KeyInput::Left => Cell::new(cell.col.saturating_sub(1), cell.row),
                    KeyInput::Right => {
                        Cell::new((cell.col + 1).min(cols.saturating_sub(1)), cell.row)
                    }
                    KeyInput::Up => Cell::new(cell.col, cell.row.saturating_sub(1)),
                    _ => Cell::new(cell.col, (cell.row + 1).min(rows.saturating_sub(1))),
                };
                self.keyboard_move = Some((tile, cell));
            }
            KeyInput::Enter => {
                self.keyboard_move = None;
                actions.push(FieldAction::Apply(FieldOp::Move {
                    id: tile,
                    landing: Landing {
                        container: self.zoomed,
                        cell,
                    },
                }));
            }
            KeyInput::Escape => self.keyboard_move = None,
            _ => {}
        }
    }

    fn handle_pointer(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let whole = ui.painter.bounds();
        let held = ui.input.pressing(whole);
        let Some((cx, cy)) = ui.input.cursor() else {
            if self.drag != Drag::Idle {
                self.finish_drag(ui, doc, data, actions);
            }
            return;
        };
        let sample = DragSample {
            x: cx as f32,
            y: cy as f32,
            t_ms: data.now_ms,
        };
        match (self.drag, held) {
            (Drag::Idle, true) => {
                // Topmost tile whose grab band is under the pointer: later in reading order
                // draws later, so search from the end.
                let mut hit = None;
                for (id, rect) in self.current_rects(doc, ui).into_iter().rev() {
                    if Self::band(rect, ui).contains(cx, cy) {
                        hit = Some((id, rect));
                        break;
                    }
                }
                if let Some((id, rect)) = hit {
                    self.drag.press(
                        id,
                        GrabOffset {
                            dx: (cx - rect.x) as f32,
                            dy: (cy - rect.y) as f32,
                        },
                        sample,
                    );
                    self.focus = Some(id);
                }
            }
            (Drag::Idle, false) => {}
            (_, true) => {
                self.drag.sample(sample, &self.drag_cfg);
            }
            (_, false) => self.finish_drag(ui, doc, data, actions),
        }
    }

    fn finish_drag(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let bounds = self.bounds;
        let pixel_bounds = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        match self.drag.release(&self.drag_cfg, pixel_bounds) {
            None => {}
            Some(Release::Click { tile }) => {
                // A click on the band selects. The click itself is left for the band's own
                // controls (a session's "park" button) to claim when the tile is painted.
                self.focus = Some(tile);
            }
            Some(Release::Drop {
                tile,
                tile_at,
                thrown,
                lifted_for_ms,
            }) => {
                let _ = ui.input.take_click(ui.painter.bounds());
                if let Some(op) = self.drop_op(doc, tile, tile_at, ui, data) {
                    let relift = self.last_drop.is_some_and(|(last, at)| {
                        last == tile && data.now_ms.saturating_sub(at) < 3000
                    });
                    self.last_drop = Some((tile, data.now_ms));
                    actions.push(FieldAction::DropMetrics(DropMetrics {
                        thrown,
                        lifted_for_ms,
                        semantic: !matches!(op, FieldOp::Move { .. }),
                        relift,
                        op: op.label(),
                    }));
                    actions.push(FieldAction::Apply(op));
                }
            }
        }
    }

    /// The cell a tile dropped with its top-left at `tile_at` lands on: the cell under the
    /// tile's first cell's centre.
    fn landing_cell(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
    ) -> Option<Cell> {
        let bounds = to_pixel(self.bounds);
        let gap = ui.px(GAP);
        let (cols, rows) = self.grid_dims(doc);
        let one = lege_field::cell_rect(cols, rows, bounds, Cell::new(0, 0), Span::new(1, 1), gap);
        let _ = tile;
        cell_at(
            cols,
            rows,
            bounds,
            tile_at.0 as i32 + one.w / 2,
            tile_at.1 as i32 + one.h / 2,
            gap,
        )
    }

    /// The target under the pointer for the grammar, and the op a drop there performs. Inside a
    /// zoomed project every drop is placement on its grid: containers are one level deep, so
    /// there is nothing else a drop there could mean.
    fn target_at(doc: &FieldDocument, grid: Option<TileId>, source: TileId, cell: Cell) -> Target {
        if grid.is_some() {
            return Target::Empty {
                container: grid,
                cell,
            };
        }
        match doc.tile_at(None, cell).filter(|id| *id != source) {
            Some(id) => match doc.kind_of(id) {
                Some(TileKind::Project { .. }) => Target::Project { id },
                Some(kind) => Target::Tile {
                    id,
                    kind: kind.clone(),
                    accepts_open: false,
                },
                None => Target::Empty {
                    container: None,
                    cell,
                },
            },
            None => Target::Empty {
                container: None,
                cell,
            },
        }
    }

    fn op_for(doc: &FieldDocument, source: TileId, operation: &Operation) -> Option<FieldOp> {
        Some(match operation {
            Operation::Relocate { container, to } => FieldOp::Move {
                id: source,
                landing: Landing {
                    container: *container,
                    cell: *to,
                },
            },
            Operation::Stash => FieldOp::Stash { id: source },
            Operation::GroupIntoNewProject { with } => {
                let target_cell = doc.location(*with)?.placement.cell;
                let project_id = format!("field-{}", doc.next_tile_id().0);
                let project = Tile {
                    id: field_model::project_tile_id(&project_id),
                    kind: TileKind::Project { project_id },
                    title: "New project".to_owned(),
                    spans: SpanRange::for_kind(
                        &TileKind::Project {
                            project_id: String::new(),
                        },
                        doc.cols,
                        doc.rows,
                    ),
                    accent: None,
                };
                FieldOp::CreateProject {
                    project,
                    landing: Landing {
                        container: None,
                        cell: target_cell,
                    },
                    absorb: vec![*with, source],
                }
            }
            Operation::PinTool { project }
            | Operation::ParkSession { project }
            | Operation::ScopeWidget { project }
            | Operation::AddToProject { project } => FieldOp::Move {
                id: source,
                landing: Landing {
                    container: Some(*project),
                    cell: Cell::new(0, 0),
                },
            },
            // Opening a file with an app and adding a reference to a tray need services this
            // screen does not reach yet (trays and file tiles arrive with persistence); until
            // then these are placement only, which is exactly what an unlabelled drop means.
            Operation::OpenWith { .. } | Operation::AddReference { .. } => return None,
        })
    }

    fn drop_op(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
        _data: &FieldData<'_>,
    ) -> Option<FieldOp> {
        let kind = doc.kind_of(tile)?;
        let cell = self.landing_cell(doc, tile, tile_at, ui)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        let operation = lege_field::operation_for(tile, kind, &target)?;
        Self::op_for(doc, tile, &operation).or(Some(FieldOp::Move {
            id: tile,
            landing: Landing {
                container: self.zoomed,
                cell,
            },
        }))
    }

    /// The ghost for the current lift (pointer or keyboard): a dry run of the drop.
    fn ghost(&self, doc: &FieldDocument, ui: &Ui<'_>) -> Option<Ghost> {
        let (tile, cell, label_anchor) = if let Some((tile, cell)) = self.keyboard_move {
            (tile, cell, None)
        } else if self.drag.is_lifted() {
            let tile = self.drag.tile()?;
            let pixel_bounds = (
                self.bounds.x as f32,
                self.bounds.y as f32,
                self.bounds.w as f32,
                self.bounds.h as f32,
            );
            let at = self.drag.projected_position(&self.drag_cfg, pixel_bounds)?;
            let cell = self.landing_cell(doc, tile, at, ui)?;
            (tile, cell, Some(cell))
        } else {
            return None;
        };
        let kind = doc.kind_of(tile)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        let operation = lege_field::operation_for(tile, kind, &target)?;
        let label = operation.label();
        let label_at = match (&target, label_anchor) {
            (Target::Tile { id, .. } | Target::Project { id }, Some(_)) => self
                .tile_rect(doc, *id, ui)
                .map(|rect| Rect::new(rect.x, rect.y, rect.w, ui.px(22).min(rect.h))),
            _ => None,
        };
        let op = Self::op_for(doc, tile, &operation).unwrap_or(FieldOp::Move {
            id: tile,
            landing: Landing {
                container: self.zoomed,
                cell,
            },
        });
        let (next, _) = apply(doc, &op).ok()?;
        let placement = match &op {
            FieldOp::CreateProject { project, .. } => next.location(project.id)?.placement,
            _ => next.location(tile)?.placement,
        };
        let moved = doc
            .located
            .iter()
            .filter(|(id, _)| *id != tile)
            .filter_map(|(id, before)| {
                let after = next.location(*id)?;
                (after.container == self.zoomed && after.placement != before.placement)
                    .then_some((*id, after.placement))
            })
            .collect();
        Some(Ghost {
            placement,
            moved,
            label: if matches!(op, FieldOp::CreateProject { .. }) {
                Some(op.label())
            } else {
                label
            },
            label_at,
        })
    }

    fn primary_action(
        &mut self,
        tile: &Tile,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        match &tile.kind {
            TileKind::App { app_id } => actions.push(FieldAction::Launch(app_id.clone())),
            TileKind::ParkedSession { .. } => {
                if let Some(window) =
                    field_model::window_for_tile(data.field, tile.id, data.toplevels)
                {
                    actions.push(if window.shelved {
                        FieldAction::Restore(window.window_id)
                    } else {
                        FieldAction::Activate(window.window_id)
                    });
                }
            }
            TileKind::Widget { widget, .. } => {
                if let Some(drawer) = field_model::widget_drawer(widget) {
                    self.expanded = Some(drawer);
                    self.drawer_view.scroll = pixelkit_ui::ScrollState::new();
                }
            }
            TileKind::Project { .. } => {
                self.zoomed = Some(tile.id);
                self.focus = None;
                self.keyboard_move = None;
            }
            TileKind::Tray { .. } | TileKind::File { .. } | TileKind::System { .. } => {}
        }
    }

    #[allow(clippy::too_many_lines)]
    fn paint_tile(
        &mut self,
        ui: &mut Ui<'_>,
        rect: Rect,
        tile: &Tile,
        visual: TileVisual,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let radius = ui.theme.corner_radius as f32;
        let window = match tile.kind {
            TileKind::ParkedSession { .. } => {
                field_model::window_for_tile(data.field, tile.id, data.toplevels)
            }
            _ => None,
        };
        let running = match &tile.kind {
            TileKind::App { app_id } => data
                .toplevels
                .iter()
                .filter(|t| t.canonical_app_id.as_ref() == Some(app_id))
                .count(),
            _ => 0,
        };

        if visual.lifted {
            ui.painter
                .shadow_rect(rect, ui.px(6), ui.px(10), 0x0000_0000, 120);
        }
        let fill = match &tile.kind {
            TileKind::Project { .. } => ui.theme.accent_soft,
            TileKind::ParkedSession { .. } => {
                if window.is_some_and(|w| w.shelved) {
                    ui.theme.panel_alt
                } else {
                    ui.theme.panel
                }
            }
            TileKind::Widget { .. } | TileKind::System { .. } => ui.theme.panel_alt,
            TileKind::App { .. } | TileKind::Tray { .. } | TileKind::File { .. } => ui.theme.panel,
        };
        let fill = if visual.hovered && !visual.lifted {
            ui.theme.hover
        } else {
            fill
        };
        ui.painter
            .fill_rounded_rect_aa(ui.kernel, rect, radius, fill, 255);
        let edge = match window {
            Some(w) if !w.shelved => ui.theme.good,
            _ if visual.focused => ui.theme.accent,
            _ => ui.theme.panel_edge,
        };
        ui.painter.stroke_rect(
            rect,
            if visual.focused || window.is_some_and(|w| !w.shelved) {
                ui.px(2)
            } else {
                ui.scale.hairline()
            },
            edge,
        );
        if let Some(cell) = visual.displaced {
            let _ = cell;
            ui.painter.blend_rect(rect, ui.theme.background, 90);
        }

        let band = Self::band(rect, ui);
        let band_style = TextStyle::new(ui.theme.body.face, 13.0);
        // `split_right` hands back the right strip first.
        let (band_controls, title_area) = band.split_right(ui.px(44).min(band.w / 2));
        ui.label_styled(
            title_area.inset(ui.px(6)),
            &tile.title,
            band_style,
            ui.theme.text,
            Align::Left,
        );
        let content = Rect::new(rect.x, band.bottom(), rect.w, (rect.h - band.h).max(0));

        match &tile.kind {
            TileKind::App { app_id } => {
                let size = ui
                    .px(40)
                    .min(content.w - ui.px(8))
                    .min(content.h - ui.px(8))
                    .max(8);
                let icon = Rect::new(
                    content.x + (content.w - size) / 2,
                    content.y + (content.h - size) / 2,
                    size,
                    size,
                );
                if let Some(bitmap) = data.icons.get(&app_id.to_string()) {
                    ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
                } else {
                    ui.painter.fill_rounded_rect_aa(
                        ui.kernel,
                        icon,
                        radius,
                        ui.theme.accent_soft,
                        255,
                    );
                    let initial = tile
                        .title
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string();
                    ui.label(icon, &initial, ui.theme.accent, Align::Centre);
                }
                if running > 0 {
                    let badge = Rect::new(
                        rect.right() - ui.px(22),
                        rect.y + ui.px(4),
                        ui.px(18),
                        ui.px(16),
                    );
                    ui.badge(
                        badge,
                        &running.to_string(),
                        ui.theme.text,
                        ui.theme.good_soft,
                        ui.theme.good,
                    );
                }
            }
            TileKind::ParkedSession { app_id, identifier } => {
                let inner = content.inset(ui.px(6));
                if let Some(preview) = data.previews.get(identifier) {
                    ui.painter.blit(preview, inner, ScaleFilter::Bilinear);
                } else if let Some(bitmap) = app_id
                    .as_ref()
                    .and_then(|id| data.icons.get(&id.to_string()))
                {
                    let size = ui.px(32).min(inner.w).min(inner.h).max(4);
                    let icon = Rect::new(
                        inner.x + ui.px(4),
                        inner.y + (inner.h - size) / 2,
                        size,
                        size,
                    );
                    ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
                }
                let state = match window {
                    Some(w) if w.shelved => "parked",
                    Some(w) if w.maximized => "open · maximized",
                    Some(_) => "open",
                    None => "closed",
                };
                let small = TextStyle::new(ui.theme.body.face, 11.0);
                let state_area = Rect::new(inner.x, inner.bottom() - ui.px(16), inner.w, ui.px(16));
                ui.label_styled(state_area, state, small, ui.theme.text_dim, Align::Right);
                if let Some(w) = window
                    && !w.shelved
                    && band_controls.w >= ui.px(30)
                {
                    let park = band_controls.inset(ui.px(3));
                    let hovered = ui.input.hovering(park);
                    ui.painter.fill_rounded_rect(
                        park,
                        ui.theme.corner_radius,
                        if hovered {
                            ui.theme.selection
                        } else {
                            ui.theme.panel_edge
                        },
                    );
                    ui.label_styled(park, "park", small, ui.theme.text, Align::Centre);
                    if ui.input.take_click(park) {
                        actions.push(FieldAction::Park(w.window_id));
                    }
                }
            }
            TileKind::Widget { widget, .. } => {
                let small = TextStyle::new(ui.theme.body.face, 12.0);
                let drawer = field_model::widget_drawer(widget);
                let inner = content.inset(ui.px(6));
                // A directory drawer tall enough shows its directory as a cushion treemap —
                // the one place on the field where area means bytes; compact, it shows a
                // composition band under its lines instead.
                let treemap_drawer = drawer
                    .filter(|d| matches!(d, Drawer::Downloads | Drawer::Documents | Drawer::Media));
                let tall = inner.h >= ui.px(60) && inner.w >= ui.px(60);
                if let Some(d) = treemap_drawer
                    && tall
                {
                    let wanted = (inner.w.max(1) as u32, inner.h.max(1) as u32);
                    match data.treemaps.get(&d) {
                        Some(bitmap) if close_enough(bitmap, wanted) => {
                            ui.painter.blit(bitmap, inner, ScaleFilter::Bilinear);
                        }
                        _ => {
                            actions.push(FieldAction::NeedTreemap {
                                drawer: d,
                                width: wanted.0,
                                height: wanted.1,
                            });
                            ui.label_styled(
                                inner,
                                "scanning…",
                                small,
                                ui.theme.text_dim,
                                Align::Left,
                            );
                        }
                    }
                } else {
                    let lines = drawer
                        .and_then(|drawer| data.drawer_texts.get(&drawer))
                        .map_or(&[][..], Vec::as_slice);
                    let band = treemap_drawer
                        .and_then(|d| data.compositions.get(&d))
                        .filter(|c| !c.is_empty());
                    let band_height = if band.is_some() { ui.px(6) } else { 0 };
                    let text_area =
                        Rect::new(inner.x, inner.y, inner.w, (inner.h - band_height).max(0));
                    let line_height = ui.px(16);
                    let mut y = text_area.y;
                    ui.painter.push_clip(text_area);
                    for line in lines
                        .iter()
                        .take((text_area.h / line_height).max(0) as usize)
                    {
                        let row = Rect::new(text_area.x, y, text_area.w, line_height);
                        ui.label_styled(row, line, small, ui.theme.text_dim, Align::Left);
                        y += line_height;
                    }
                    ui.painter.pop_clip();
                    if lines.is_empty() {
                        ui.label_styled(text_area, "…", small, ui.theme.text_dim, Align::Left);
                    }
                    if let Some(composition) = band {
                        let mut x = inner.x;
                        let bottom = inner.bottom() - band_height;
                        for (colour, share) in composition {
                            let w = ((inner.w as f32) * share).round() as i32;
                            if w <= 0 {
                                continue;
                            }
                            ui.painter.fill_rect(
                                Rect::new(x, bottom, w.min(inner.right() - x), band_height),
                                *colour,
                            );
                            x += w;
                        }
                    }
                }
            }
            TileKind::Project { .. } => {
                // Miniature of the interior: each child as a small chip with its title, in the
                // child grid's own proportions, so the tile reads as what it contains.
                let small = TextStyle::new(ui.theme.body.face, 11.0);
                let inner = content.inset(ui.px(6));
                if let Some(container) = data.field.container(tile.id) {
                    let children = data.field.placements_in(Some(tile.id));
                    ui.painter.push_clip(inner);
                    for (child, placement) in &children {
                        let chip = from_pixel(lege_field::cell_rect(
                            container.cols,
                            container.rows,
                            to_pixel(inner),
                            placement.cell,
                            placement.span,
                            ui.px(3),
                        ));
                        ui.painter
                            .fill_rounded_rect(chip, ui.px(3), ui.theme.panel_edge);
                        if let Some(child_tile) = data.field.tile(*child) {
                            ui.label_styled(
                                chip.inset(ui.px(3)),
                                &child_tile.title,
                                small,
                                ui.theme.text,
                                Align::Left,
                            );
                        }
                    }
                    ui.painter.pop_clip();
                    if children.is_empty() {
                        ui.label_styled(
                            inner,
                            "empty — drop tiles here",
                            small,
                            ui.theme.text_dim,
                            Align::Left,
                        );
                    }
                }
            }
            TileKind::Tray { .. } | TileKind::File { .. } | TileKind::System { .. } => {
                let small = TextStyle::new(ui.theme.body.face, 12.0);
                ui.label_styled(
                    content.inset(ui.px(6)),
                    tile.kind.name(),
                    small,
                    ui.theme.text_dim,
                    Align::Left,
                );
            }
        }

        if !visual.lifted && ui.input.take_click(content) {
            self.focus = Some(tile.id);
            self.primary_action(tile, data, actions);
        }
    }
}

/// Whether a cached treemap is near enough to `wanted` to blit rather than rasterize again:
/// within a quarter in each dimension, so a drag's every pixel of resize does not re-scan.
fn close_enough(bitmap: &Bitmap, wanted: (u32, u32)) -> bool {
    let near = |have: u32, want: u32| have * 4 >= want * 3 && have * 3 <= want * 4;
    near(bitmap.width, wanted.0) && near(bitmap.height, wanted.1)
}

#[derive(Clone, Copy, Debug)]
struct TileVisual {
    hovered: bool,
    focused: bool,
    lifted: bool,
    /// Drawn dimmed: a ghost predicts it moving to this cell.
    displaced: Option<Cell>,
}

/// The first tile in reading order among `rects`.
fn first_in(rects: &[(TileId, Rect)]) -> Option<TileId> {
    rects.first().map(|(id, _)| *id)
}

/// The nearest tile in `direction` from `from`, by centre, weighting the primary axis — the
/// same rule as `lege_field::focus::neighbour`, over whichever grid is on screen.
fn neighbour_in(rects: &[(TileId, Rect)], from: TileId, direction: Direction) -> Option<TileId> {
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = (origin.x + origin.w / 2, origin.y + origin.h / 2);
    let mut best: Option<(i64, TileId)> = None;
    for (id, rect) in rects {
        if *id == from {
            continue;
        }
        let (cx, cy) = (rect.x + rect.w / 2, rect.y + rect.h / 2);
        let (ahead, aside) = match direction {
            Direction::Left => (ox - cx, (cy - oy).abs()),
            Direction::Right => (cx - ox, (cy - oy).abs()),
            Direction::Up => (oy - cy, (cx - ox).abs()),
            Direction::Down => (cy - oy, (cx - ox).abs()),
        };
        if ahead <= 0 {
            continue;
        }
        let score = i64::from(ahead) + 2 * i64::from(aside);
        if best.is_none_or(|(s, b)| (score, *id) < (s, b)) {
            best = Some((score, *id));
        }
    }
    best.map(|(_, id)| id)
}

fn to_pixel(rect: Rect) -> PixelRect {
    PixelRect::new(rect.x, rect.y, rect.w, rect.h)
}

fn from_pixel(rect: PixelRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.w, rect.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field_model::{LiveState, merge, tile_id_for};
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, Modifiers, MouseButton, Scale};
    use pixelkit_ui::Theme;

    const W: i32 = 1200;
    const H: i32 = 800;

    fn toplevel(id: u64, app: &str, shelved: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: format!("win {id}"),
            verified_identity: true,
            activated: false,
            fullscreen: false,
            shelved,
            maximized: false,
            geometry: lege_window::Rect::new(0, 0, 640, 480),
            layer: lege_window::Layer::Normal,
        }
    }

    struct Harness {
        screen: FieldScreen,
        doc: FieldDocument,
        toplevels: Vec<ShellToplevel>,
        text: pixelkit_text::TextCache,
        kernel: RasterKernel,
        buffer: WindowBuffer,
        input: Input,
        now: u64,
    }

    impl Harness {
        fn new() -> Self {
            let (cols, rows) = field_model::grid_for(W, H, field_model::TILE_SIZE);
            let mut doc = FieldDocument::new("test", cols, rows);
            let apps: Vec<AppId> = vec!["org.legeos.Files".parse().unwrap()];
            let toplevels = vec![
                toplevel(1, "org.legeos.Files", false),
                toplevel(2, "org.example.Editor", true),
            ];
            merge(
                &mut doc,
                &LiveState {
                    apps: &apps,
                    toplevels: &toplevels,
                    projects: &[],
                },
            );
            Harness {
                screen: FieldScreen::default(),
                doc,
                toplevels,
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(W as u32, H as u32),
                input: Input::new(),
                now: 0,
            }
        }

        fn frame(&mut self) -> Vec<FieldAction> {
            self.frame_raw()
                .into_iter()
                .filter(|action| {
                    !matches!(
                        action,
                        FieldAction::NeedTreemap { .. } | FieldAction::DropMetrics(_)
                    )
                })
                .collect()
        }

        /// Every action, background requests and experiment bookkeeping included.
        fn frame_raw(&mut self) -> Vec<FieldAction> {
            self.now += 16;
            let icons = HashMap::new();
            let previews = HashMap::new();
            let drawer_texts = HashMap::new();
            let treemaps = HashMap::new();
            let compositions = HashMap::new();
            let data = FieldData {
                field: &self.doc,
                toplevels: &self.toplevels,
                icons: &icons,
                previews: &previews,
                drawer_texts: &drawer_texts,
                treemaps: &treemaps,
                compositions: &compositions,
                search_results: None,
                connected: true,
                home_revealed: false,
                now_ms: self.now,
            };
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                Theme::default(),
                Scale::ONE,
                &mut self.kernel,
            );
            let actions = self.screen.draw(&mut ui, Rect::new(0, 0, W, H), &data);
            self.input.end_frame();
            actions
        }

        fn rect_of(&mut self, tile: TileId) -> Rect {
            // A frame to establish bounds, then the rect.
            self.frame();
            let ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                Theme::default(),
                Scale::ONE,
                &mut self.kernel,
            );
            self.screen
                .tile_rect(&self.doc, tile, &ui)
                .expect("tile is placed")
        }

        fn click(&mut self, x: i32, y: i32) -> Vec<FieldAction> {
            self.input.cursor_moved(x as f32, y as f32);
            self.input.mouse(MouseButton::Left, true);
            self.input.mouse(MouseButton::Left, false);
            self.frame()
        }

        fn key(&mut self, key: KeyInput) -> Vec<FieldAction> {
            self.input.key(key);
            self.frame()
        }

        fn apply(&mut self, actions: &[FieldAction]) {
            for action in actions {
                if let FieldAction::Apply(op) = action {
                    self.doc = apply(&self.doc, op).expect("op applies").0;
                }
            }
        }
    }

    #[test]
    fn clicking_an_app_tile_launches_it_and_a_session_tile_restores_or_activates() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.legeos.Files");
        let rect = h.rect_of(files);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(
            actions,
            vec![FieldAction::Launch("org.legeos.Files".parse().unwrap())]
        );

        let parked = field_model::session_tile_for(&h.doc, &h.toplevels[1]).unwrap();
        let rect = h.rect_of(parked);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(actions, vec![FieldAction::Restore(WindowId(2))]);

        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(actions, vec![FieldAction::Activate(WindowId(1))]);
    }

    #[test]
    fn the_park_control_on_an_open_session_parks_it() {
        let mut h = Harness::new();
        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        let actions = h.click(rect.right() - 20, rect.y + 10);
        assert_eq!(actions, vec![FieldAction::Park(WindowId(1))]);
    }

    #[test]
    fn clicking_a_drawer_tile_expands_it_and_escape_returns() {
        let mut h = Harness::new();
        let inbox = tile_id_for("drawer:Downloads");
        let rect = h.rect_of(inbox);
        h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(h.screen.expanded, Some(Drawer::Downloads));
        h.key(KeyInput::Escape);
        assert_eq!(h.screen.expanded, None);
    }

    #[test]
    fn field_keyboard_reaches_every_action() {
        let mut h = Harness::new();
        // Arrow: focus the first tile (Downloads, top-left); Enter opens it; Escape closes.
        h.key(KeyInput::Right);
        assert_eq!(h.screen.focus, Some(tile_id_for("drawer:Downloads")));
        h.key(KeyInput::Enter);
        assert_eq!(h.screen.expanded, Some(Drawer::Downloads));
        h.key(KeyInput::Escape);
        assert_eq!(h.screen.expanded, None);

        // Walk right to the Files app tile and launch it with Enter.
        let files = tile_id_for("app:org.legeos.Files");
        for _ in 0..3 {
            h.key(KeyInput::Right);
        }
        assert_eq!(h.screen.focus, Some(files));
        let actions = h.key(KeyInput::Enter);
        assert_eq!(
            actions,
            vec![FieldAction::Launch("org.legeos.Files".parse().unwrap())]
        );

        // Space lifts, arrows move the ghost, Enter drops: an undoable Move.
        let before = h.doc.location(files).unwrap().placement.cell;
        h.key(KeyInput::Character(' '));
        assert!(h.screen.keyboard_move.is_some());
        h.key(KeyInput::Right);
        h.key(KeyInput::Down);
        let actions = h.key(KeyInput::Enter);
        assert!(matches!(
            actions.as_slice(),
            [FieldAction::Apply(FieldOp::Move { .. })]
        ));
        h.apply(&actions);
        let after = h.doc.location(files).unwrap().placement.cell;
        assert_eq!(after, Cell::new(before.col + 1, before.row + 1));

        // Ctrl+Z asks for undo; Delete stashes the focused tile.
        h.input.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        let actions = h.key(KeyInput::Character('z'));
        assert_eq!(actions, vec![FieldAction::Undo]);
        h.input.modifiers_changed(Modifiers::default());
        let actions = h.key(KeyInput::Delete);
        assert_eq!(
            actions,
            vec![FieldAction::Apply(FieldOp::Stash { id: files })]
        );
    }

    #[test]
    fn dragging_a_tile_by_its_band_moves_it_and_shows_a_ghost_first() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.legeos.Files");
        let rect = h.rect_of(files);
        let grab = (rect.x + 10, rect.y + 8);
        h.input.cursor_moved(grab.0 as f32, grab.1 as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(matches!(h.screen.drag, Drag::Pressed { .. }));
        // Drag three cells right, in steps so velocity stays low.
        let (cols, rows) = (h.doc.cols, h.doc.rows);
        let one = lege_field::cell_rect(
            cols,
            rows,
            to_pixel(h.screen.bounds),
            Cell::new(0, 0),
            Span::new(1, 1),
            GAP,
        );
        let dx = (one.w + GAP) * 3;
        for step in 1..=10 {
            h.input
                .cursor_moved((grab.0 + dx * step / 10) as f32, grab.1 as f32);
            h.now += 100;
            h.frame();
        }
        assert!(h.screen.drag.is_lifted());
        assert!(h.screen.animating());
        let before = h.doc.location(files).unwrap().placement.cell;
        h.input.mouse(MouseButton::Left, false);
        let raw: Vec<FieldAction> = h
            .frame_raw()
            .into_iter()
            .filter(|a| !matches!(a, FieldAction::NeedTreemap { .. }))
            .collect();
        let [
            FieldAction::DropMetrics(metrics),
            FieldAction::Apply(FieldOp::Move { id, landing }),
        ] = raw.as_slice()
        else {
            panic!("expected the drop's metrics then one Move, got {raw:?}");
        };
        assert!(!metrics.thrown, "throwing is off by default");
        assert!(!metrics.semantic, "a move is placement only");
        assert!(!metrics.relift);
        assert!(
            metrics.lifted_for_ms >= 900,
            "ten 100 ms steps in the air: {metrics:?}"
        );
        assert_eq!(*id, files);
        assert_eq!(landing.cell, Cell::new(before.col + 3, before.row));
        assert_eq!(h.screen.drag, Drag::Idle);
    }

    #[test]
    fn a_project_zooms_in_and_its_children_can_be_moved_within_and_back_out() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.legeos.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let op = FieldOp::CreateProject {
            project: Tile {
                id: field_model::project_tile_id("field-1"),
                kind: TileKind::Project {
                    project_id: "field-1".into(),
                },
                title: "Pair".into(),
                spans: field_model::project_spans(h.doc.cols, h.doc.rows),
                accent: None,
            },
            landing: Landing {
                container: None,
                cell: Cell::new(6, 2),
            },
            absorb: vec![files, editor],
        };
        h.apply(&[FieldAction::Apply(op)]);
        let project = field_model::project_tile_id("field-1");

        // Click the project's body: zoomed in, children have rects, the field's tiles do not.
        let rect = h.rect_of(project);
        h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(h.screen.zoomed, Some(project));
        h.frame();
        assert!(h.rect_of(files).w > 0);
        let inbox = tile_id_for("drawer:Downloads");
        h.frame();
        {
            let ui = Ui::new(
                Painter::new(&mut h.buffer),
                &mut h.text,
                &mut h.input,
                Theme::default(),
                Scale::ONE,
                &mut h.kernel,
            );
            assert_eq!(h.screen.tile_rect(&h.doc, inbox, &ui), None);
        }

        // Keyboard move inside the project stays inside it.
        h.key(KeyInput::Right);
        assert_eq!(h.screen.focus, Some(files).or(Some(editor)));
        let focused = h.screen.focus.unwrap();
        h.key(KeyInput::Character(' '));
        h.key(KeyInput::Down);
        let actions = h.key(KeyInput::Enter);
        let [FieldAction::Apply(FieldOp::Move { id, landing })] = actions.as_slice() else {
            panic!("expected a Move, got {actions:?}");
        };
        assert_eq!(*id, focused);
        assert_eq!(landing.container, Some(project));
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(focused), Some(project));

        // Backspace moves it back out next to the project; Escape leaves the zoom.
        h.frame();
        h.screen.focus = Some(focused);
        let actions = h.key(KeyInput::Backspace);
        let [FieldAction::Apply(FieldOp::Move { landing, .. })] = actions.as_slice() else {
            panic!("expected a Move out, got {actions:?}");
        };
        assert_eq!(landing.container, None);
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(focused), None);
        h.key(KeyInput::Escape);
        assert_eq!(h.screen.zoomed, None);
    }

    #[test]
    fn dropping_one_loose_tile_on_another_offers_a_new_project() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.legeos.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let from = h.rect_of(files);
        let to = h.rect_of(editor);
        h.input
            .cursor_moved((from.x + 10) as f32, (from.y + 8) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        for step in 1..=10 {
            let x = from.x + 10 + (to.x - from.x) * step / 10;
            h.input.cursor_moved(x as f32, (from.y + 8) as f32);
            h.now += 100;
            h.frame();
        }
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let [FieldAction::Apply(FieldOp::CreateProject { absorb, .. })] = actions.as_slice() else {
            panic!("expected CreateProject, got {actions:?}");
        };
        assert_eq!(absorb, &vec![editor, files]);
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(files), h.doc.parent_of(editor));
        assert!(h.doc.parent_of(files).is_some());
    }
}
