//! Pixelkit rendering for the standalone manager.

use lege_client::files::{EntryKind, FolderSummary, RootKind, VolumeInfo};
use pixelkit_raster::{Painter, RasterKernel, Rect, ScaleFilter, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, Scale};
use pixelkit_text::{Align, TextCache, TextStyle};
use pixelkit_ui::{ScrollState, Theme, Ui};

use crate::{
    actions::{ActionId, Command, MenuTarget, shortcut},
    fonts, format,
    model::{BrowserModel, SelectionModifiers},
};

const HEADER_HEIGHT: i32 = 54;
const TOOLBAR_HEIGHT: i32 = 42;
const STATUS_HEIGHT: i32 = 34;
const ROOT_WIDTH: i32 = 224;
const PROPERTIES_WIDTH: i32 = 286;
const ROOT_ROW_HEIGHT: i32 = 58;
const ENTRY_ROW_HEIGHT: i32 = 38;
const COLUMN_HEIGHT: i32 = 28;

const GROUND: u32 = 0x00ec_eef0;
const PANEL: u32 = 0x00fb_fcfd;
const PANEL_ALT: u32 = 0x00f3_f5f7;
const EDGE: u32 = 0x00d5_dae0;
const TEXT: u32 = 0x0017_1b20;
const TEXT_DIM: u32 = 0x0069_737d;
const ACCENT: u32 = 0x000e_7482;
const ACCENT_SOFT: u32 = 0x00e3_f1f3;
const DANGER: u32 = 0x00b3_261e;
const GOOD: u32 = 0x001f_8f60;
/// The Home Field's folder material (`design::FOLDER_SHELL`/`FOLDER_EDGE`), so a folder reads
/// as the same warm paper in both surfaces.
const FOLDER_PLATE: u32 = 0x00fb_f8f1;
const FOLDER_PLATE_EDGE: u32 = 0x00e2_dbc9;

fn draw_place_icon(ui: &mut Ui<'_>, area: Rect, drive: bool) {
    let side = ui.px(20);
    let icon = Rect::new(area.x, area.y + ui.px(3), side, ui.px(16));
    if !drive {
        ui.painter.fill_rounded_rect(
            Rect::new(icon.x, icon.y, side / 2, ui.px(6)),
            ui.px(2),
            ACCENT,
        );
    }
    ui.painter.fill_rounded_rect(
        Rect::new(icon.x, icon.y + ui.px(4), side, ui.px(12)),
        ui.px(2),
        if drive { TEXT_DIM } else { ACCENT },
    );
}

#[derive(Debug)]
struct FileMenu {
    at: (i32, i32),
    rows: Vec<(&'static str, Command)>,
}

impl FileMenu {
    fn new(at: (i32, i32), target: &MenuTarget, model: &BrowserModel) -> Self {
        let action = |id| Command::Action(id, SelectionModifiers::default());
        let mut rows = Vec::new();
        match target {
            MenuTarget::Bookmark(path) => {
                rows.push(("Open", Command::Navigate(path.clone())));
                rows.push(("Open in new window", Command::OpenWindow(path.clone())));
                rows.push(("Add to Home", Command::AddToHome(path.clone())));
                rows.push((
                    "Remove bookmark",
                    Command::Bookmark {
                        path: path.clone(),
                        enabled: false,
                    },
                ));
            }
            MenuTarget::Entry(index) => {
                if let Some(entry) = model.listing.as_ref().and_then(|l| l.entries.get(*index)) {
                    rows.push(("Open", action(ActionId::Open)));
                    if entry.is_dir() {
                        rows.push((
                            "Open in new window",
                            Command::OpenWindow(entry.path.clone()),
                        ));
                    }
                    rows.push(("Copy", action(ActionId::Copy)));
                    rows.push(("Cut", action(ActionId::Cut)));
                    if entry.is_dir() {
                        rows.push(("Paste into folder", Command::PasteInto(entry.path.clone())));
                        rows.push((
                            "Bookmark folder",
                            Command::Bookmark {
                                path: entry.path.clone(),
                                enabled: true,
                            },
                        ));
                        rows.push(("Add to Home", Command::AddToHome(entry.path.clone())));
                    }
                    rows.push(("Rename", action(ActionId::Rename)));
                    rows.push(("Move to Trash", action(ActionId::Trash)));
                    rows.push(("Properties", action(ActionId::Properties)));
                }
            }
            MenuTarget::Ground => {
                rows.push(("Paste", action(ActionId::Paste)));
                rows.push(("Select all", action(ActionId::SelectAll)));
                if let Some(path) = model.directory() {
                    rows.push((
                        "Open in new window",
                        Command::OpenWindow(path.to_path_buf()),
                    ));
                    rows.push((
                        "Bookmark folder",
                        Command::Bookmark {
                            path: path.to_path_buf(),
                            enabled: true,
                        },
                    ));
                    rows.push(("Add to Home", Command::AddToHome(path.to_path_buf())));
                }
            }
        }
        Self { at, rows }
    }

    fn rect(&self, ui: &Ui<'_>, bounds: Rect) -> Rect {
        let w = ui.px(230).min(bounds.w);
        let h = (self.rows.len() as i32 * ui.px(32)).min(bounds.h);
        Rect::new(
            self.at.0.clamp(bounds.x, bounds.right() - w),
            self.at.1.clamp(bounds.y, bounds.bottom() - h),
            w,
            h,
        )
    }

    fn update(&self, ui: &mut Ui<'_>, bounds: Rect) -> (Option<Command>, bool) {
        let escape = ui.input.take_keys().contains(&KeyInput::Escape);
        let rect = self.rect(ui, bounds);
        for (index, (_, command)) in self.rows.iter().enumerate() {
            let row = Rect::new(rect.x, rect.y + index as i32 * ui.px(32), rect.w, ui.px(32));
            if ui.input.take_click(row) {
                return (Some(command.clone()), true);
            }
        }
        (
            None,
            escape || ui.input.take_click(bounds) || ui.input.take_context_click(bounds),
        )
    }

    fn paint(&self, ui: &mut Ui<'_>, bounds: Rect) {
        let rect = self.rect(ui, bounds);
        ui.painter.fill_rect(rect, PANEL);
        ui.painter.stroke_rect(rect, ui.scale.hairline(), EDGE);
        for (index, (label, _)) in self.rows.iter().enumerate() {
            let row = Rect::new(rect.x, rect.y + index as i32 * ui.px(32), rect.w, ui.px(32));
            if ui.input.hovering(row) {
                ui.painter.fill_rect(row.inset(1), ACCENT_SOFT);
            }
            ui.label(row.inset(ui.px(6)), label, TEXT, Align::Left);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Dialog {
    Rename { text: String, error: Option<String> },
    Location { text: String, error: Option<String> },
}

pub struct FileSurface {
    text: TextCache,
    kernel: RasterKernel,
    pub input: Input,
    roots_scroll: ScrollState,
    entries_scroll: ScrollState,
    dialog: Option<Dialog>,
    menu: Option<FileMenu>,
}

impl std::fmt::Debug for FileSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSurface")
            .field("dialog", &self.dialog)
            .finish_non_exhaustive()
    }
}

impl Default for FileSurface {
    fn default() -> Self {
        Self {
            text: TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            input: Input::new(),
            roots_scroll: ScrollState::new(),
            entries_scroll: ScrollState::new(),
            dialog: None,
            menu: None,
        }
    }
}

impl FileSurface {
    pub fn begin_rename(&mut self, current_name: String) {
        self.dialog = Some(Dialog::Rename {
            text: current_name,
            error: None,
        });
    }

    pub fn begin_location(&mut self, current: String) {
        self.dialog = Some(Dialog::Location {
            text: current,
            error: None,
        });
    }

    pub fn dismiss_dialog(&mut self) {
        self.dialog = None;
    }

    pub fn set_dialog_error(&mut self, message: String) {
        match &mut self.dialog {
            Some(Dialog::Rename { error, .. } | Dialog::Location { error, .. }) => {
                *error = Some(message);
            }
            None => {}
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(
        &mut self,
        buffer: &mut WindowBuffer,
        scale: Scale,
        model: &BrowserModel,
    ) -> Vec<Command> {
        let mut ui = Ui::new(
            Painter::new(buffer),
            &mut self.text,
            &mut self.input,
            theme(),
            scale,
            &mut self.kernel,
        );
        ui.painter.clear(GROUND);
        let whole = Rect::new(0, 0, ui.painter.width(), ui.painter.height());
        let mut commands = if let Some(menu) = &mut self.menu {
            let (chosen, close) = menu.update(&mut ui, whole);
            if close {
                self.menu = None;
            }
            chosen.into_iter().collect()
        } else if let Some(dialog) = &mut self.dialog {
            handle_dialog_keys(&mut ui, dialog)
        } else {
            let modifiers = ui.input.modifiers();
            ui.input
                .take_keys()
                .into_iter()
                .filter_map(|key| shortcut(&key, modifiers))
                .map(|action| {
                    Command::Action(
                        action,
                        SelectionModifiers {
                            control: modifiers.control,
                            shift: modifiers.shift,
                        },
                    )
                })
                .collect()
        };

        let whole = Rect::new(0, 0, ui.painter.width(), ui.painter.height());
        let (header, below_header) = whole.split_top(ui.px(HEADER_HEIGHT));
        let (navigation, below_navigation) = below_header.split_top(ui.px(TOOLBAR_HEIGHT));
        let (operations, below_toolbars) = below_navigation.split_top(ui.px(TOOLBAR_HEIGHT));
        let (status, body) = below_toolbars.split_bottom(ui.px(STATUS_HEIGHT));

        draw_header(&mut ui, header, model);
        commands.extend(draw_navigation(&mut ui, navigation, model));
        commands.extend(draw_operations(&mut ui, operations, model));

        let left_width = ui.px(ROOT_WIDTH).min((body.w / 3).max(ui.px(150)));
        let right_width = ui.px(PROPERTIES_WIDTH).min((body.w / 3).max(ui.px(190)));
        let (roots, rest) = body.split_left(left_width);
        let (properties, entries) = rest.split_right(right_width);
        commands.extend(draw_roots(&mut ui, roots, model, &mut self.roots_scroll));
        commands.extend(draw_entries(
            &mut ui,
            entries,
            model,
            &mut self.entries_scroll,
        ));
        if !model.in_trash() && ui.input.take_context_click(entries) {
            commands.push(Command::ContextMenu {
                at: ui.input.cursor().unwrap_or((entries.x, entries.y)),
                target: MenuTarget::Ground,
            });
        }
        draw_properties(&mut ui, properties, model);
        commands.extend(draw_status(&mut ui, status, model));

        if let Some(dialog) = &mut self.dialog {
            commands.extend(draw_dialog(&mut ui, whole, dialog));
        }
        commands.retain(|command| {
            if let Command::ContextMenu { at, target } = command {
                self.menu = Some(FileMenu::new(*at, target, model));
                false
            } else {
                true
            }
        });
        if let Some(menu) = &self.menu {
            menu.paint(&mut ui, whole);
        }
        ui.input.end_frame();
        commands
    }
}

fn theme() -> Theme {
    let mut theme = Theme::default();
    theme.background = GROUND;
    theme.panel = PANEL;
    theme.panel_alt = PANEL_ALT;
    theme.panel_edge = EDGE;
    theme.text = TEXT;
    theme.text_dim = TEXT_DIM;
    theme.accent = ACCENT;
    theme.accent_soft = ACCENT_SOFT;
    theme.selection = ACCENT_SOFT;
    theme.hover = 0x00ff_ffff;
    theme.alarm = DANGER;
    theme.good = GOOD;
    theme.body = theme.body.with_size(13.0);
    theme.heading = theme.heading.with_size(24.0);
    theme.padding = 10;
    theme.corner_radius = 6;
    theme
}

fn draw_header(ui: &mut Ui<'_>, area: Rect, model: &BrowserModel) {
    ui.painter.fill_rect(area, PANEL);
    ui.painter.fill_rect(
        Rect::new(
            area.x,
            area.bottom() - ui.scale.hairline(),
            area.w,
            ui.scale.hairline(),
        ),
        EDGE,
    );
    let (title, detail) = area.inset(ui.px(12)).split_left(ui.px(180));
    ui.label_styled(
        title,
        "FILES",
        TextStyle::tracked(fonts::BODY, 20.0, 0.10),
        TEXT,
        Align::Left,
    );
    let path = model.pending_path().or_else(|| model.directory());
    let volume = model.volume.as_ref().map(|volume| {
        format!(
            "{} free of {} · {}{}",
            format::bytes(volume.free_bytes),
            format::bytes(volume.total_bytes),
            volume.fs_type,
            if volume.read_only {
                " · READ-ONLY"
            } else {
                ""
            }
        )
    });
    ui.label(
        detail,
        &format!(
            "{}{}",
            path.map_or_else(
                || "Waiting for a location".to_owned(),
                |path| path.display().to_string()
            ),
            volume.map_or_else(String::new, |volume| format!("   ·   {volume}"))
        ),
        TEXT_DIM,
        Align::Left,
    );
}

fn draw_navigation(ui: &mut Ui<'_>, area: Rect, model: &BrowserModel) -> Vec<Command> {
    ui.painter.fill_rect(area, PANEL_ALT);
    let mut commands = Vec::new();
    let mut cursor = area.inset(ui.px(4));
    for (label, action, width) in [
        ("Back", ActionId::Back, 72),
        ("Forward", ActionId::Forward, 86),
        ("Up", ActionId::Up, 58),
        ("Refresh", ActionId::Refresh, 84),
        ("Location", ActionId::Location, 92),
    ] {
        let (button, rest) = cursor.split_left(ui.px(width));
        if ui.button(button.inset(ui.px(3)), label) {
            commands.push(Command::Action(action, SelectionModifiers::default()));
        }
        cursor = rest;
    }
    let (hidden, rest) = cursor.split_right(ui.px(128));
    let (view, rest) = rest.split_right(ui.px(72));
    let (direction, rest) = rest.split_right(ui.px(92));
    let (sort, path) = rest.split_right(ui.px(64));
    for (button, action) in [
        (sort, ActionId::CycleSort),
        (direction, ActionId::ToggleSortDirection),
        (view, ActionId::ToggleView),
    ] {
        if ui.button(button.inset(ui.px(3)), action.label()) {
            commands.push(Command::Action(action, SelectionModifiers::default()));
        }
    }
    ui.label(
        path.inset(ui.px(8)),
        &if model.in_trash() {
            "Trash".to_owned()
        } else {
            model
                .pending_path()
                .or_else(|| model.directory())
                .map_or_else(
                    || "No location".to_owned(),
                    |path| path.display().to_string(),
                )
        },
        TEXT,
        Align::Left,
    );
    if ui.button(
        hidden.inset(ui.px(3)),
        if model.show_hidden {
            "Hidden: on"
        } else {
            "Hidden: off"
        },
    ) {
        commands.push(Command::Action(
            ActionId::ToggleHidden,
            SelectionModifiers::default(),
        ));
    }
    commands
}

fn draw_operations(ui: &mut Ui<'_>, area: Rect, model: &BrowserModel) -> Vec<Command> {
    ui.painter.fill_rect(area, PANEL);
    let mut commands = Vec::new();
    let mut cursor = area.inset(ui.px(4));
    // The trash is a place, not a folder, so it gets its own toolbar: there is no paste into
    // the trash and no rename of something deleted, and offering either would be a button that
    // can only fail. Undo stays, because restoring is undoable like any other operation.
    let mut actions = if model.in_trash() {
        vec![
            ("Restore", ActionId::Restore, 82),
            ("Delete permanently", ActionId::DeletePermanently, 148),
            ("Empty trash", ActionId::EmptyTrash, 104),
            ("Undo", ActionId::Undo, 66),
        ]
    } else {
        vec![
            ("Open", ActionId::Open, 68),
            ("Copy", ActionId::Copy, 68),
            ("Copy path", ActionId::CopyPath, 92),
            ("Cut", ActionId::Cut, 58),
            ("Paste", ActionId::Paste, 70),
            ("Rename", ActionId::Rename, 82),
            ("Trash", ActionId::Trash, 70),
            ("Undo", ActionId::Undo, 66),
            ("Properties", ActionId::Properties, 96),
        ]
    };
    if model.active_operation_id.is_some() {
        actions.push(("Cancel", ActionId::CancelOperation, 74));
    }
    for (label, action, width) in actions {
        let (button, rest) = cursor.split_left(ui.px(width));
        if ui.button(button.inset(ui.px(3)), label) {
            commands.push(Command::Action(action, SelectionModifiers::default()));
        }
        cursor = rest;
    }
    ui.label(
        cursor.inset(ui.px(8)),
        &selection_summary(model),
        TEXT_DIM,
        Align::Right,
    );
    commands
}

fn selection_summary(model: &BrowserModel) -> String {
    match model.selected.len() {
        0 => model.clipboard.as_ref().map_or_else(
            || "Select an item for properties and actions".to_owned(),
            |clipboard| format!("{} item(s) on the clipboard", clipboard.paths.len()),
        ),
        1 => model.single_selected().map_or_else(
            || "1 selected".to_owned(),
            |path| {
                format!(
                    "Selected: {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            },
        ),
        count => format!("{count} selected"),
    }
}

fn draw_roots(
    ui: &mut Ui<'_>,
    area: Rect,
    model: &BrowserModel,
    scroll: &mut ScrollState,
) -> Vec<Command> {
    ui.painter.fill_rect(area, PANEL_ALT);
    let (title, body) = area.inset(ui.px(6)).split_top(ui.px(30));
    ui.label_styled(
        title,
        "PLACES & DRIVES",
        TextStyle::tracked(fonts::BODY, 11.0, 0.12),
        TEXT_DIM,
        Align::Left,
    );
    // The trash sits above the drives as its own place, because that is what it is: not a folder
    // on any one of them, but one listing over every trash the user has.
    let (trash_row, body) = body.split_top(ui.px(ROOT_ROW_HEIGHT));
    let mut commands = Vec::new();
    if model.in_trash() {
        ui.painter.fill_rect(trash_row, ACCENT_SOFT);
    } else if ui.input.hovering(trash_row) {
        ui.painter.fill_rect(trash_row, 0x00ff_ffff);
    }
    {
        let (label, detail) = trash_row.inset(ui.px(6)).split_top(ui.px(22));
        ui.label(label, "Trash", TEXT, Align::Left);
        ui.label_styled(
            detail,
            &match model.trash.as_ref() {
                Some(view) if view.loading => "Loading…".to_owned(),
                Some(view) => format!("{} item(s)", view.entries.len()),
                None => "Deleted items".to_owned(),
            },
            TextStyle::new(fonts::BODY, 11.0),
            TEXT_DIM,
            Align::Left,
        );
        if ui.input.take_click(trash_row) {
            commands.push(Command::Action(
                ActionId::OpenTrash,
                SelectionModifiers::default(),
            ));
        }
    }
    let heights = vec![ui.px(ROOT_ROW_HEIGHT); model.sources.roots.len()];
    let home = model.sources.home();
    ui.scroll_list(scroll, body, &heights, |ui, index, row| {
        let root = &model.sources.roots[index];
        // Not "the last folder I was in" — while the trash is open, no drive is the open place.
        let active = !model.in_trash()
            && model
                .directory()
                .is_some_and(|directory| directory == root.path);
        if active {
            ui.painter.fill_rect(row, ACCENT_SOFT);
        } else if ui.input.hovering(row) && root.available {
            ui.painter.fill_rect(row, 0x00ff_ffff);
        }
        // The name leads. A PLACE or DRIVE caption in front of every row was the same word
        // repeated down the rail, and a place's path underneath it was the home folder repeated
        // once per row; the detail line now says only what the name does not — free space for a
        // drive, and a path only where it is not the obvious one under the home folder.
        let (icon, text) = row.inset(ui.px(6)).split_left(ui.px(28));
        draw_place_icon(ui, icon, root.kind == RootKind::Drive);
        let (label, detail) = text.split_top(ui.px(22));
        ui.label(
            label,
            &root.label,
            if root.available { TEXT } else { TEXT_DIM },
            Align::Left,
        );
        let detail_text = root.volume.as_ref().map_or_else(
            || {
                if !root.available {
                    "Not present".to_owned()
                } else if root.kind == RootKind::Place && is_under(&root.path, home) {
                    String::new()
                } else {
                    root.path.display().to_string()
                }
            },
            |volume| {
                format!(
                    "{} free · {}",
                    format::bytes(volume.free_bytes),
                    volume.fs_type
                )
            },
        );
        ui.label_styled(
            detail,
            &detail_text,
            TextStyle::new(fonts::BODY, 11.0),
            TEXT_DIM,
            Align::Left,
        );
        if root.key.starts_with("bookmark:") && ui.input.take_context_click(row) {
            commands.push(Command::ContextMenu {
                at: ui.input.cursor().unwrap_or((row.x, row.y)),
                target: MenuTarget::Bookmark(root.path.clone()),
            });
        }
        if root.available && ui.input.take_click(row) {
            commands.push(Command::Navigate(root.path.clone()));
        }
    });
    commands
}

/// A secondary click on entry `index`: select it first unless it already is (a right-click on
/// one of several selected rows keeps the set), then open the menu at the pointer.
fn entry_menu(ui: &Ui<'_>, at: Rect, index: usize, model: &BrowserModel) -> Vec<Command> {
    let mut commands = Vec::new();
    let selected = model
        .listing
        .as_ref()
        .and_then(|listing| listing.entries.get(index))
        .is_some_and(|entry| model.selected.contains(&entry.path));
    if !selected {
        commands.push(Command::Select {
            index,
            modifiers: SelectionModifiers::default(),
            activate_if_selected: false,
        });
    }
    commands.push(Command::ContextMenu {
        at: ui.input.cursor().unwrap_or((at.x, at.y)),
        target: MenuTarget::Entry(index),
    });
    commands
}

/// Whether `path` is the home folder or sits directly inside it — the places whose path the
/// rail need not spell, because the name already does.
fn is_under(path: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    home.is_some_and(|home| path == home || path.parent() == Some(home))
}

/// The main pane: whichever listing is the open place. The two are held apart rather than
/// squeezed into one row-drawing loop, because they are different listings with different
/// columns and different things a click can mean.
fn draw_entries(
    ui: &mut Ui<'_>,
    area: Rect,
    model: &BrowserModel,
    scroll: &mut ScrollState,
) -> Vec<Command> {
    ui.painter.fill_rect(area, PANEL);
    let (columns, body) = area.split_top(ui.px(COLUMN_HEIGHT));
    if model.in_trash() {
        draw_trash_columns(ui, columns);
        return draw_trash_entries(ui, body, model, scroll);
    }
    if model.view == lege_types::file_sort::ViewMode::Thumbnails {
        return draw_thumbnail_grid(ui, area, model, scroll);
    }
    draw_columns(ui, columns);
    draw_directory_entries(ui, body, model, scroll)
}

/// The same rows as the detail list, drawn as responsive preview cells. Selection, focus and
/// activation behave exactly like list rows — a cell click is a `Command::Select` on the same
/// index — and the grid asks for its visible thumbnails through `Command::ThumbnailsVisible`,
/// never by touching a file itself.
fn draw_thumbnail_grid(
    ui: &mut Ui<'_>,
    area: Rect,
    model: &BrowserModel,
    scroll: &mut ScrollState,
) -> Vec<Command> {
    let Some(listing) = &model.listing else {
        ui.label(
            area.inset(ui.px(14)),
            "Loading folder contents…",
            TEXT_DIM,
            Align::Left,
        );
        return Vec::new();
    };
    if listing.entries.is_empty() {
        ui.label(
            area.inset(ui.px(14)),
            "This folder is empty.",
            TEXT_DIM,
            Align::Left,
        );
        return Vec::new();
    }
    let cell = ui.px(132).max(48);
    let columns = ((area.w / cell).max(1)) as usize;
    let rows = listing.entries.len().div_ceil(columns);
    let heights = vec![cell; rows];
    let mut commands = Vec::new();
    let mut seen: Option<(usize, usize)> = None;
    ui.scroll_list(scroll, area, &heights, |ui, row_index, row| {
        for slot in 0..columns {
            let index = row_index * columns + slot;
            let Some(entry) = listing.entries.get(index) else {
                break;
            };
            seen = Some(match seen {
                None => (index, index + 1),
                Some((low, high)) => (low.min(index), high.max(index + 1)),
            });
            let slot_i = i32::try_from(slot).unwrap_or(0);
            let tile = Rect::new(row.x + slot_i * cell, row.y, cell, row.h).inset(ui.px(4));
            let selected = model.selected.contains(&entry.path);
            if selected {
                ui.painter.fill_rect(tile, ACCENT_SOFT);
            } else if ui.input.hovering(tile) {
                ui.painter.fill_rect(tile, 0x00f7_f9fa);
            } else {
                ui.painter.fill_rect(tile, PANEL_ALT);
            }
            if index == model.focus {
                ui.painter
                    .fill_rect(Rect::new(tile.x, tile.y, ui.px(3), tile.h), ACCENT);
            }
            let (image, caption) = tile.split_bottom(ui.px(20));
            if let Some(bitmap) = model.thumbnails.get(&entry.path) {
                ui.painter.blit(
                    bitmap,
                    image.inset(ui.px(2)),
                    pixelkit_raster::ScaleFilter::Bilinear,
                );
            } else if entry.is_dir() {
                // Folder-ness is shown, not spelled: the warm folder plate Home uses for
                // folder objects, with no text repeating what the shape already says.
                let plate = image.inset(ui.px(6));
                ui.painter.fill_rounded_rect(plate, ui.px(6), FOLDER_PLATE);
                ui.painter
                    .stroke_rect(plate, ui.scale.hairline(), FOLDER_PLATE_EDGE);
            } else {
                // A file's extension names it once; one without shows nothing.
                let chip = entry
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map_or_else(String::new, str::to_uppercase);
                ui.label(image, &chip, TEXT_DIM, Align::Centre);
            }
            ui.label(caption.inset(ui.px(2)), &entry.name, TEXT, Align::Left);
            if ui.input.take_context_click(tile) {
                commands.extend(entry_menu(ui, tile, index, model));
            }
            if ui.input.take_click(tile) {
                commands.push(Command::Select {
                    index,
                    modifiers: SelectionModifiers {
                        control: ui.input.modifiers().control,
                        shift: ui.input.modifiers().shift,
                    },
                    activate_if_selected: true,
                });
            }
        }
    });
    if let Some((low, high)) = seen {
        commands.push(Command::ThumbnailsVisible(
            low.saturating_sub(columns)..(high + columns).min(listing.entries.len()),
        ));
    }
    commands
}

fn draw_directory_entries(
    ui: &mut Ui<'_>,
    body: Rect,
    model: &BrowserModel,
    scroll: &mut ScrollState,
) -> Vec<Command> {
    let Some(listing) = &model.listing else {
        ui.label(
            body.inset(ui.px(14)),
            "Loading folder contents…",
            TEXT_DIM,
            Align::Left,
        );
        return Vec::new();
    };
    if listing.entries.is_empty() {
        ui.label(
            body.inset(ui.px(14)),
            "This folder is empty.",
            TEXT_DIM,
            Align::Left,
        );
        return Vec::new();
    }
    let heights = vec![ui.px(ENTRY_ROW_HEIGHT); listing.entries.len()];
    let mut commands = Vec::new();
    ui.scroll_list(scroll, body, &heights, |ui, index, row| {
        let entry = &listing.entries[index];
        let selected = model.selected.contains(&entry.path);
        if selected {
            ui.painter.fill_rect(row, ACCENT_SOFT);
            if index == model.focus {
                ui.painter
                    .fill_rect(Rect::new(row.x, row.y, ui.px(3), row.h), ACCENT);
            }
        } else if ui.input.hovering(row) {
            ui.painter.fill_rect(row, 0x00f7_f9fa);
        } else if index % 2 == 1 {
            ui.painter.fill_rect(row, PANEL_ALT);
        }
        let (modified, rest) = row.split_right(ui.px(130).min(row.w / 3));
        let (size, name) = rest.split_right(ui.px(90).min(rest.w / 3));
        // The name leads; a trailing "/" is the whole folder marker and "→" the whole symlink
        // one — the size column's "—" already tells folders from files.
        let marker = if entry.kind == EntryKind::Directory {
            "/"
        } else {
            ""
        };
        ui.label(
            name.inset(ui.px(7)),
            &format!(
                "{}{marker}{}",
                entry.name,
                if entry.is_symlink { " →" } else { "" },
            ),
            TEXT,
            Align::Left,
        );
        ui.label(
            size.inset(ui.px(7)),
            &if entry.is_dir() {
                "—".into()
            } else {
                format::bytes(entry.size_bytes)
            },
            TEXT_DIM,
            Align::Right,
        );
        ui.label_styled(
            modified.inset(ui.px(7)),
            &format::timestamp_ms(entry.mtime_ms),
            TextStyle::new(fonts::BODY, 11.0),
            TEXT_DIM,
            Align::Right,
        );
        if ui.input.take_click(row) {
            let modifiers = ui.input.modifiers();
            commands.push(Command::Select {
                index,
                modifiers: SelectionModifiers {
                    control: modifiers.control,
                    shift: modifiers.shift,
                },
                activate_if_selected: true,
            });
        } else if ui.input.take_context_click(row) {
            commands.extend(entry_menu(ui, row, index, model));
        }
    });
    if listing.truncated {
        let note = Rect::new(body.x, body.bottom() - ui.px(24), body.w, ui.px(24));
        ui.painter.fill_rect(note, PANEL_ALT);
        ui.label(
            note.inset(ui.px(5)),
            "Listing capped at 500 entries.",
            TEXT_DIM,
            Align::Right,
        );
    }
    commands
}

/// The trash's own columns. A trash listing is not a directory listing: what a person needs is
/// when it went and, above all, where it came from — a modified time and a size answer neither
/// "is this the one I meant" nor "where will Restore put it back".
fn draw_trash_columns(ui: &mut Ui<'_>, area: Rect) {
    ui.painter.fill_rect(area, PANEL_ALT);
    let (deleted, rest) = area.split_right(ui.px(150).min(area.w / 3));
    let (origin, name) = rest.split_right(ui.px(240).min(rest.w / 2));
    let style = TextStyle::tracked(fonts::BODY, 10.0, 0.10);
    ui.label_styled(name.inset(ui.px(7)), "NAME", style, TEXT_DIM, Align::Left);
    ui.label_styled(
        origin.inset(ui.px(7)),
        "ORIGINAL LOCATION",
        style,
        TEXT_DIM,
        Align::Left,
    );
    ui.label_styled(
        deleted.inset(ui.px(7)),
        "DELETED",
        style,
        TEXT_DIM,
        Align::Right,
    );
}

fn draw_trash_entries(
    ui: &mut Ui<'_>,
    body: Rect,
    model: &BrowserModel,
    scroll: &mut ScrollState,
) -> Vec<Command> {
    let entries = model.trash_entries();
    if entries.is_empty() {
        let view = model.trash.as_ref();
        ui.label(
            body.inset(ui.px(14)),
            match view {
                Some(view) if view.loading => "Loading the trash…",
                Some(view) if view.error.is_some() => "The trash could not be read.",
                _ => "The trash is empty.",
            },
            TEXT_DIM,
            Align::Left,
        );
        return Vec::new();
    }
    let heights = vec![ui.px(ENTRY_ROW_HEIGHT); entries.len()];
    let mut commands = Vec::new();
    ui.scroll_list(scroll, body, &heights, |ui, index, row| {
        let entry = &entries[index];
        let selected = model.selected.contains(&entry.trashed_path);
        if selected {
            ui.painter.fill_rect(row, ACCENT_SOFT);
            if index == model.focus {
                ui.painter
                    .fill_rect(Rect::new(row.x, row.y, ui.px(3), row.h), ACCENT);
            }
        } else if ui.input.hovering(row) {
            ui.painter.fill_rect(row, 0x00f7_f9fa);
        } else if index % 2 == 1 {
            ui.painter.fill_rect(row, PANEL_ALT);
        }
        let (deleted, rest) = row.split_right(ui.px(150).min(row.w / 3));
        let (origin, name) = rest.split_right(ui.px(240).min(rest.w / 2));
        ui.label(
            name.inset(ui.px(7)),
            &format!(
                "{}   {}",
                if entry.is_directory { "FOLDER" } else { "FILE" },
                entry.name
            ),
            TEXT,
            Align::Left,
        );
        ui.label_styled(
            origin.inset(ui.px(7)),
            &entry.original_location(),
            TextStyle::new(fonts::BODY, 11.0),
            TEXT_DIM,
            Align::Left,
        );
        ui.label_styled(
            deleted.inset(ui.px(7)),
            &entry.deletion_date.replace('T', "  "),
            TextStyle::new(fonts::BODY, 11.0),
            TEXT_DIM,
            Align::Right,
        );
        // Selecting only. Double-clicking a trashed item must not open it: opening would take
        // it out of the trash's vocabulary and the whole point of the place is that its items
        // are not in use.
        if ui.input.take_click(row) || ui.input.take_context_click(row) {
            let modifiers = ui.input.modifiers();
            commands.push(Command::Select {
                index,
                modifiers: SelectionModifiers {
                    control: modifiers.control,
                    shift: modifiers.shift,
                },
                activate_if_selected: false,
            });
        }
    });
    commands
}

fn draw_columns(ui: &mut Ui<'_>, area: Rect) {
    ui.painter.fill_rect(area, PANEL_ALT);
    let (modified, rest) = area.split_right(ui.px(130).min(area.w / 3));
    let (size, name) = rest.split_right(ui.px(90).min(rest.w / 3));
    let style = TextStyle::tracked(fonts::BODY, 10.0, 0.10);
    ui.label_styled(name.inset(ui.px(7)), "NAME", style, TEXT_DIM, Align::Left);
    ui.label_styled(size.inset(ui.px(7)), "SIZE", style, TEXT_DIM, Align::Right);
    ui.label_styled(
        modified.inset(ui.px(7)),
        "MODIFIED",
        style,
        TEXT_DIM,
        Align::Right,
    );
}

#[allow(clippy::too_many_lines)]
fn draw_properties(ui: &mut Ui<'_>, area: Rect, model: &BrowserModel) {
    ui.painter.fill_rect(area, PANEL_ALT);
    let mut y = area.y + ui.px(10);
    property_line(ui, area, &mut y, "PROPERTIES", TEXT, true);
    match model.selected.len() {
        0 => {
            property_line(ui, area, &mut y, "No item selected", TEXT_DIM, false);
            if let Some(volume) = &model.volume {
                y += ui.px(8);
                draw_volume_properties(ui, area, &mut y, volume);
            }
            y += ui.px(8);
            property_line(ui, area, &mut y, "SHORTCUTS", TEXT, true);
            for line in [
                "Enter · open",
                "F2 · rename",
                "Delete · Trash",
                "Ctrl+C / X / V · transfer",
                "Ctrl+Shift+C · copy path",
                "Ctrl+Z · undo",
                "Alt+Enter · properties",
                "Alt+Left / Right · history",
                "Ctrl+L · location",
            ] {
                property_line(ui, area, &mut y, line, TEXT_DIM, false);
            }
        }
        count if count > 1 => {
            property_line(
                ui,
                area,
                &mut y,
                &format!("{count} items selected"),
                TEXT,
                false,
            );
            property_line(
                ui,
                area,
                &mut y,
                "Copy, move or Trash them together.",
                TEXT_DIM,
                false,
            );
        }
        _ => match (&model.properties, &model.properties_error) {
            (Some(properties), _) => {
                let info = &properties.info;
                property_line(ui, area, &mut y, &info.name, TEXT, false);
                property_line(
                    ui,
                    area,
                    &mut y,
                    match info.kind {
                        EntryKind::Directory => "Folder",
                        EntryKind::File => "File",
                        EntryKind::Other => "Special item",
                    },
                    TEXT_DIM,
                    false,
                );
                property_line(
                    ui,
                    area,
                    &mut y,
                    &info.path.display().to_string(),
                    TEXT_DIM,
                    false,
                );
                draw_selected_preview(ui, area, &mut y, model);
                y += ui.px(8);
                property_line(ui, area, &mut y, "SIZE", TEXT, true);
                if let Some(folder) = &info.folder {
                    let (counts, sizes, warning) = folder_summary_text(folder);
                    property_line(ui, area, &mut y, &counts, TEXT_DIM, false);
                    property_line(ui, area, &mut y, &sizes, TEXT_DIM, false);
                    if let Some(warning) = warning {
                        property_line(ui, area, &mut y, &warning, DANGER, false);
                    }
                } else {
                    property_line(
                        ui,
                        area,
                        &mut y,
                        &format!(
                            "{} · {} allocated",
                            format::bytes(info.size_bytes),
                            format::bytes(info.allocated_bytes)
                        ),
                        TEXT_DIM,
                        false,
                    );
                }
                property_line(ui, area, &mut y, "MODIFIED", TEXT, true);
                property_line(
                    ui,
                    area,
                    &mut y,
                    &format::timestamp_ms(info.modified_ms),
                    TEXT_DIM,
                    false,
                );
                property_line(ui, area, &mut y, "PERMISSIONS", TEXT, true);
                property_line(
                    ui,
                    area,
                    &mut y,
                    &format!(
                        "{} · uid {} · gid {}",
                        format::mode(info.mode),
                        info.uid,
                        info.gid
                    ),
                    TEXT_DIM,
                    false,
                );
                if let Some(target) = &info.symlink_target {
                    property_line(ui, area, &mut y, "LINK TARGET", TEXT, true);
                    property_line(
                        ui,
                        area,
                        &mut y,
                        &target.display().to_string(),
                        TEXT_DIM,
                        false,
                    );
                }
                y += ui.px(8);
                draw_volume_properties(ui, area, &mut y, &properties.volume);
            }
            (_, Some(error)) => property_line(ui, area, &mut y, error, DANGER, false),
            _ => {
                draw_selected_preview(ui, area, &mut y, model);
                property_line(
                    ui,
                    area,
                    &mut y,
                    "Choose Properties or press Alt+Enter.",
                    TEXT_DIM,
                    false,
                );
            }
        },
    }
}

fn folder_summary_text(folder: &FolderSummary) -> (String, String, Option<String>) {
    (
        format!(
            "{} file(s) · {} folder(s)",
            folder.file_count, folder.directory_count
        ),
        format!(
            "{}{} · {} allocated",
            if folder.complete { "" } else { "At least " },
            format::bytes(folder.total_size_bytes),
            format::bytes(folder.total_allocated_bytes)
        ),
        (!folder.complete).then(|| {
            format!(
                "Partial · {} unreadable folder(s)",
                folder.unreadable_directories
            )
        }),
    )
}

fn draw_selected_preview(ui: &mut Ui<'_>, area: Rect, y: &mut i32, model: &BrowserModel) {
    let Some(preview) = &model.preview else {
        if let Some(error) = &model.preview_error {
            property_line(ui, area, y, "PREVIEW", TEXT, true);
            property_line(ui, area, y, error, DANGER, false);
        }
        return;
    };
    property_line(ui, area, y, "PREVIEW", TEXT, true);
    let available_width = (area.w - ui.px(20)).max(1);
    let available_height = ui.px(160);
    let width_scale = f64::from(available_width) / f64::from(preview.bitmap.width.max(1));
    let height_scale = f64::from(available_height) / f64::from(preview.bitmap.height.max(1));
    let scale = width_scale.min(height_scale).min(1.0);
    let width = (f64::from(preview.bitmap.width) * scale).round().max(1.0) as i32;
    let height = (f64::from(preview.bitmap.height) * scale).round().max(1.0) as i32;
    let preview_area = Rect::new(area.x + (area.w - width) / 2, *y, width, height);
    ui.painter.fill_rect(preview_area, PANEL);
    ui.painter
        .blit(&preview.bitmap, preview_area, ScaleFilter::Bilinear);
    *y += height + ui.px(4);
    property_line(
        ui,
        area,
        y,
        &format!(
            "{}×{} · {}",
            preview.metadata.width, preview.metadata.height, preview.metadata.renderer
        ),
        TEXT_DIM,
        false,
    );
}

fn property_line(ui: &mut Ui<'_>, area: Rect, y: &mut i32, text: &str, colour: u32, heading: bool) {
    let height = ui.px(if heading { 24 } else { 22 });
    ui.label_styled(
        Rect::new(area.x + ui.px(10), *y, area.w - ui.px(20), height),
        text,
        if heading {
            TextStyle::tracked(fonts::BODY, 10.0, 0.11)
        } else {
            TextStyle::new(fonts::BODY, 12.0)
        },
        colour,
        Align::Left,
    );
    *y += height;
}

fn draw_volume_properties(ui: &mut Ui<'_>, area: Rect, y: &mut i32, volume: &VolumeInfo) {
    property_line(ui, area, y, "CONTAINING DRIVE", TEXT, true);
    property_line(
        ui,
        area,
        y,
        &volume.mount_point.display().to_string(),
        TEXT_DIM,
        false,
    );
    property_line(
        ui,
        area,
        y,
        &format!(
            "{} · {} free",
            volume.fs_type,
            format::bytes(volume.free_bytes)
        ),
        TEXT_DIM,
        false,
    );
    property_line(
        ui,
        area,
        y,
        &format!(
            "{} reserved for recovery",
            format::bytes(volume.reserved_bytes)
        ),
        TEXT_DIM,
        false,
    );
    if volume.read_only {
        property_line(ui, area, y, "READ-ONLY", DANGER, true);
    }
}

fn draw_status(ui: &mut Ui<'_>, area: Rect, model: &BrowserModel) -> Vec<Command> {
    ui.painter.fill_rect(area, PANEL);
    let message = model
        .error
        .as_deref()
        .or(model.status.as_deref())
        .unwrap_or("Ready");
    ui.label(
        area.inset(ui.px(8)),
        message,
        if model.error.is_some() {
            DANGER
        } else {
            TEXT_DIM
        },
        Align::Left,
    );
    if let Some(operation) = &model.operation {
        let width = area.w.min(ui.px(220));
        let track = Rect::new(
            area.right() - width - ui.px(10),
            area.y + ui.px(12),
            width,
            ui.px(8),
        );
        ui.painter.fill_rect(track, EDGE);
        ui.painter.fill_rect(
            Rect::new(
                track.x,
                track.y,
                track.w * i32::from(operation.progress_percent()) / 100,
                track.h,
            ),
            if operation.state.terminal() {
                GOOD
            } else {
                ACCENT
            },
        );
    }
    Vec::new()
}

fn handle_dialog_keys(ui: &mut Ui<'_>, dialog: &mut Dialog) -> Vec<Command> {
    let modifiers = ui.input.modifiers();
    let mut commands = Vec::new();
    for key in ui.input.take_keys() {
        let (text, error) = match dialog {
            Dialog::Rename { text, error } | Dialog::Location { text, error } => (text, error),
        };
        match key {
            KeyInput::Character(character) if !modifiers.control && !modifiers.alt => {
                text.push(character);
                *error = None;
            }
            KeyInput::Backspace => {
                text.pop();
                *error = None;
            }
            KeyInput::Escape => commands.push(Command::DismissDialog),
            KeyInput::Enter => commands.push(match dialog {
                Dialog::Rename { text, .. } => Command::ConfirmRename(text.clone()),
                Dialog::Location { text, .. } => Command::ConfirmLocation(text.clone()),
            }),
            _ => {}
        }
    }
    commands
}

fn draw_dialog(ui: &mut Ui<'_>, whole: Rect, dialog: &mut Dialog) -> Vec<Command> {
    ui.painter.blend_rect(whole, 0x0017_1b20, 88);
    let width = ui.px(520).min(whole.w - ui.px(30));
    let height = ui.px(178).min(whole.h - ui.px(30));
    let panel = Rect::new(
        whole.x + (whole.w - width) / 2,
        whole.y + (whole.h - height) / 2,
        width,
        height,
    );
    ui.painter.fill_rect(panel, PANEL);
    ui.painter.stroke_rect(panel, ui.scale.hairline(), EDGE);
    let (title, text, error, buttons) = match dialog {
        Dialog::Rename { text, error } => ("RENAME", text, error.as_deref(), "Rename"),
        Dialog::Location { text, error } => ("OPEN LOCATION", text, error.as_deref(), "Open"),
    };
    ui.label_styled(
        Rect::new(
            panel.x + ui.px(14),
            panel.y + ui.px(10),
            panel.w - ui.px(28),
            ui.px(28),
        ),
        title,
        TextStyle::tracked(fonts::BODY, 12.0, 0.12),
        TEXT,
        Align::Left,
    );
    let field = Rect::new(
        panel.x + ui.px(14),
        panel.y + ui.px(46),
        panel.w - ui.px(28),
        ui.px(40),
    );
    ui.painter.fill_rect(field, 0x00ff_ffff);
    ui.painter.stroke_rect(field, ui.px(2), ACCENT);
    ui.label(field.inset(ui.px(8)), text, TEXT, Align::Left);
    if let Some(error) = error {
        ui.label(
            Rect::new(
                panel.x + ui.px(14),
                panel.y + ui.px(90),
                panel.w - ui.px(28),
                ui.px(24),
            ),
            error,
            DANGER,
            Align::Left,
        );
    }
    let apply = Rect::new(
        panel.right() - ui.px(102),
        panel.bottom() - ui.px(46),
        ui.px(88),
        ui.px(34),
    );
    let cancel = Rect::new(apply.x - ui.px(92), apply.y, ui.px(82), apply.h);
    let mut commands = Vec::new();
    if ui.button(cancel, "Cancel") {
        commands.push(Command::DismissDialog);
    }
    if ui.button(apply, buttons) {
        commands.push(match dialog {
            Dialog::Rename { text, .. } => Command::ConfirmRename(text.clone()),
            Dialog::Location { text, .. } => Command::ConfirmLocation(text.clone()),
        });
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_client::files::{
        BrowseEntry, BrowserRoot, BrowserSources, DirectoryListing, PreviewKind, PreviewMetadata,
    };
    use pixelkit_shell::MouseButton;
    use std::path::PathBuf;

    fn rendered_model() -> BrowserModel {
        let volume = VolumeInfo {
            mount_point: "/".into(),
            fs_type: "btrfs".into(),
            total_bytes: 1_000,
            free_bytes: 400,
            available_bytes: 350,
            reserved_bytes: 100,
            read_only: false,
        };
        let mut model = BrowserModel::new(None, false);
        let request = model
            .set_sources(BrowserSources {
                roots: vec![BrowserRoot {
                    key: "home".into(),
                    label: "Home".into(),
                    path: "/home/u".into(),
                    kind: RootKind::Place,
                    available: true,
                    volume: None,
                }],
            })
            .unwrap();
        model.apply_browse(
            request.id,
            &request.path,
            Ok(crate::model::BrowseSnapshot {
                listing: DirectoryListing {
                    directory: PathBuf::from("/home/u"),
                    parent: Some(PathBuf::from("/home")),
                    entries: vec![BrowseEntry {
                        path: "/home/u/Documents".into(),
                        name: "Documents".into(),
                        kind: EntryKind::Directory,
                        is_symlink: false,
                        hidden: false,
                        size_bytes: 0,
                        mtime_ms: 1,
                    }],
                    truncated: false,
                    limit: 500,
                },
                volume,
            }),
        );
        model
    }

    #[test]
    fn interaction_dialogs_are_explicit_and_dismissible() {
        let mut surface = FileSurface::default();
        surface.begin_rename("old.txt".into());
        assert!(surface.dialog.is_some());
        surface.set_dialog_error("bad name".into());
        assert!(matches!(
            surface.dialog,
            Some(Dialog::Rename { error: Some(_), .. })
        ));
        surface.dismiss_dialog();
        assert!(surface.dialog.is_none());
    }

    #[test]
    fn browser_surface_renders_real_rows_and_pointer_selects_them() {
        let model = rendered_model();
        let mut surface = FileSurface::default();
        let mut buffer = WindowBuffer::new(1180, 760);
        assert!(surface.draw(&mut buffer, Scale::ONE, &model).is_empty());
        assert!(buffer.pixels.iter().any(|pixel| *pixel != GROUND));

        // Entries begin to the right of the 224 px roots rail and below 54 + 42 + 42 + 28 px.
        surface.input.cursor_moved(300.0, 180.0);
        surface.input.mouse(MouseButton::Left, true);
        let commands = surface.draw(&mut buffer, Scale::ONE, &model);
        assert!(
            matches!(commands.as_slice(), [Command::Select { index: 0, .. }]),
            "{commands:?}"
        );
    }

    #[test]
    fn trash_surface_shows_the_trash_place_its_own_columns_and_its_own_toolbar() {
        let mut model = rendered_model();
        model.open_trash();
        model.apply_trash(Ok(vec![lege_client::files::TrashEntry {
            name: "report.pdf".into(),
            trashed_path: "/home/u/.local/share/Trash/files/report.pdf".into(),
            info_path: "/home/u/.local/share/Trash/info/report.pdf.trashinfo".into(),
            original_path: "/home/u/work/report.pdf".into(),
            deletion_date: "2026-08-30T18:04:11".into(),
            trash_root: "/home/u/.local/share/Trash".into(),
            is_directory: false,
            size_bytes: 2048,
        }]));

        let mut surface = FileSurface::default();
        let mut buffer = WindowBuffer::new(1180, 760);
        assert!(surface.draw(&mut buffer, Scale::ONE, &model).is_empty());

        // The trash's row sits where the directory's did, and clicking it selects a trashed
        // item rather than a file in the folder underneath.
        surface.input.cursor_moved(300.0, 180.0);
        surface.input.mouse(MouseButton::Left, true);
        let commands = surface.draw(&mut buffer, Scale::ONE, &model);
        assert!(
            matches!(
                commands.as_slice(),
                [Command::Select {
                    index: 0,
                    activate_if_selected: false,
                    ..
                }]
            ),
            "{commands:?}"
        );

        // Empty trash is on the toolbar (second row, y = 54 + 42 .. + 42), where Copy path is
        // when a folder is open — the trash's own actions replace the folder's.
        surface.input.cursor_moved(230.0, 118.0);
        surface.input.mouse(MouseButton::Left, true);
        let commands = surface.draw(&mut buffer, Scale::ONE, &model);
        assert!(
            commands.iter().any(|command| matches!(
                command,
                Command::Action(ActionId::EmptyTrash | ActionId::DeletePermanently, _)
            )),
            "{commands:?}"
        );
    }

    #[test]
    fn preview_surface_paints_the_selected_files_bitmap() {
        let mut model = rendered_model();
        model.select(0, SelectionModifiers::default());
        model.preview = Some(crate::model::PreviewSnapshot {
            metadata: PreviewMetadata {
                renderer: "test-renderer".into(),
                kind: PreviewKind::Heic,
                width: 2,
                height: 1,
            },
            bitmap: pixelkit_raster::Bitmap {
                width: 2,
                height: 1,
                pixels: vec![0xffff_0000, 0xff00_ff00],
            },
        });
        let mut surface = FileSurface::default();
        let mut buffer = WindowBuffer::new(1180, 760);
        surface.draw(&mut buffer, Scale::ONE, &model);
        assert!(buffer.pixels.contains(&0x00ff_0000));
        assert!(buffer.pixels.contains(&0x0000_ff00));
    }

    #[test]
    fn properties_folder_summary_names_file_count_and_total_size() {
        let exact = folder_summary_text(&FolderSummary {
            file_count: 3,
            directory_count: 2,
            total_size_bytes: 21,
            total_allocated_bytes: 12_288,
            unreadable_directories: 0,
            complete: true,
        });
        assert_eq!(exact.0, "3 file(s) · 2 folder(s)");
        assert_eq!(exact.1, "21 B · 12 KiB allocated");
        assert!(exact.2.is_none());

        let partial = folder_summary_text(&FolderSummary {
            complete: false,
            unreadable_directories: 1,
            ..FolderSummary {
                file_count: 3,
                directory_count: 2,
                total_size_bytes: 21,
                total_allocated_bytes: 12_288,
                unreadable_directories: 0,
                complete: true,
            }
        });
        assert!(partial.1.starts_with("At least "));
        assert_eq!(
            partial.2.as_deref(),
            Some("Partial · 1 unreadable folder(s)")
        );
    }
}
