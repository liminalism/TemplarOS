//! The expanded Home file surface: real service entries, places/drives and direct navigation.

use std::path::{Path, PathBuf};

use lege_field::Selection;
use lege_types::file_sort::{SortDescriptor, SortField, SortItem, ViewMode, compare};
use pixelkit_raster::Bitmap;
use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::{ScrollState, Ui};
use std::collections::HashMap;

use crate::file_browser::{BrowseEntry, BrowserSources, DirectoryListing, EntryKind, RootKind};
use crate::home_surface::Drawer;

const ROOTS_WIDTH: i32 = 220;
const ROOT_ROW_HEIGHT: i32 = 46;
const TOOLBAR_HEIGHT: i32 = 42;
const COLUMNS_HEIGHT: i32 = 28;
const ENTRY_HEIGHT: i32 = 38;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileBrowserAction {
    Browse {
        drawer: Drawer,
        path: PathBuf,
        show_hidden: bool,
    },
    Open(PathBuf),
}

/// One consistent snapshot of the app-owned async state. Paths are paired with the drawer and
/// hidden-files mode that requested them so a slow response can never replace a newer view.
#[derive(Debug)]
pub struct FileBrowserData<'a> {
    pub sources: &'a BrowserSources,
    pub listing: Option<(Drawer, bool, &'a DirectoryListing)>,
    pub pending: Option<(Drawer, bool, &'a Path)>,
    pub error: Option<(Drawer, &'a Path, &'a str)>,
    pub status: Option<&'a str>,
    pub search_query: Option<&'a str>,
    pub search_entries: Option<&'a [BrowseEntry]>,
    pub search_error: Option<&'a str>,
    /// Decoded thumbnails by path, produced off the render thread by the host from the bounded
    /// requests [`FileBrowserScreen::begin_thumbnails`] hands it. Drawing only ever reads.
    pub thumbnails: &'a HashMap<PathBuf, Bitmap>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusArea {
    Roots,
    Entries,
}

#[derive(Debug)]
pub struct FileBrowserScreen {
    drawer: Option<Drawer>,
    show_hidden: bool,
    focus_area: FocusArea,
    root_focus: usize,
    entry_focus: usize,
    entry_selection: Selection<PathBuf>,
    root_scroll: ScrollState,
    entry_scroll: ScrollState,
    last_directory: Option<PathBuf>,
    /// The shared ordering contract (`lege_types::file_sort`). The default is exactly the
    /// order the service serves, so re-sorting is a pure client-side re-view.
    sort: SortDescriptor,
    view: ViewMode,
    /// Bumped whenever the directory changes, so a thumbnail decoded for the old listing is
    /// recognisably stale and dropped by the host instead of painted into the wrong folder.
    thumbnail_generation: u64,
    /// The entry range the last thumbnail draw could see, plus a row of lookahead — the only
    /// entries [`Self::begin_thumbnails`] will ask for.
    visible_thumbnails: std::ops::Range<usize>,
}

impl Default for FileBrowserScreen {
    fn default() -> Self {
        Self {
            drawer: None,
            show_hidden: false,
            focus_area: FocusArea::Entries,
            root_focus: 0,
            entry_focus: 0,
            entry_selection: Selection::default(),
            root_scroll: ScrollState::new(),
            entry_scroll: ScrollState::new(),
            last_directory: None,
            sort: SortDescriptor::default(),
            view: ViewMode::default(),
            thumbnail_generation: 0,
            visible_thumbnails: 0..0,
        }
    }
}

impl FileBrowserScreen {
    /// Paths currently selected in deterministic order, ready for a caller's bulk file action.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.entry_selection.to_vec()
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        drawer: Drawer,
        data: &FileBrowserData<'_>,
        keyboard_enabled: bool,
    ) -> Option<FileBrowserAction> {
        if self.drawer != Some(drawer) {
            self.drawer = Some(drawer);
            self.show_hidden = false;
            self.focus_area = FocusArea::Entries;
            self.entry_focus = 0;
            self.entry_selection.clear();
            self.entry_scroll = ScrollState::new();
            self.last_directory = None;
        }

        let listing = data
            .listing
            .filter(|(owner, hidden, _)| *owner == drawer && *hidden == self.show_hidden)
            .map(|(_, _, listing)| listing);
        let pending = data
            .pending
            .filter(|(owner, hidden, _)| *owner == drawer && *hidden == self.show_hidden)
            .map(|(_, _, path)| path);
        let current = pending
            .or_else(|| listing.map(|listing| listing.directory.as_path()))
            .or_else(|| data.sources.start(drawer));
        let searching = data.search_query.is_some();
        let ordered: Vec<BrowseEntry>;
        let visible_entries: &[BrowseEntry] = if searching {
            data.search_entries.unwrap_or(&[])
        } else if self.sort == SortDescriptor::default() {
            listing.map_or(&[], |listing| listing.entries.as_slice())
        } else {
            ordered = Self::sorted(
                listing.map_or(&[], |listing| listing.entries.as_slice()),
                self.sort,
            );
            &ordered
        };
        self.entry_selection
            .retain(|path| visible_entries.iter().any(|entry| &entry.path == path));

        if listing.map(|listing| &listing.directory) != self.last_directory.as_ref()
            && pending.is_none()
        {
            self.last_directory = listing.map(|listing| listing.directory.clone());
            self.entry_focus = 0;
            self.entry_selection.clear();
            self.entry_scroll = ScrollState::new();
            self.thumbnail_generation = self.thumbnail_generation.wrapping_add(1);
            self.visible_thumbnails = 0..0;
        }

        let mut action = if keyboard_enabled {
            self.handle_keys(ui, drawer, data.sources, listing, visible_entries)
        } else {
            None
        };

        ui.panel(area);
        let (roots_area, browser_area) = area.split_left(ui.px(ROOTS_WIDTH).min(area.w / 3));
        if action.is_none() {
            action = self.draw_roots(ui, roots_area, drawer, data.sources, current);
        } else {
            self.draw_roots(ui, roots_area, drawer, data.sources, current);
        }

        let browser_area = browser_area.inset(ui.px(8));
        let (toolbar, below_toolbar) = browser_area.split_top(ui.px(TOOLBAR_HEIGHT));
        if action.is_none() {
            action = self.draw_toolbar(ui, toolbar, drawer, current, listing, data.search_query);
        } else {
            self.draw_toolbar(ui, toolbar, drawer, current, listing, data.search_query);
        }
        let (columns, entries_area) = below_toolbar.split_top(ui.px(COLUMNS_HEIGHT));
        Self::draw_columns(ui, columns);

        let error = data.search_error.or_else(|| {
            data.error
                .filter(|(owner, path, _)| *owner == drawer && Some(*path) == current)
                .map(|(_, _, message)| message)
        });
        if let Some(error) = error {
            ui.label(
                entries_area.inset(ui.px(12)),
                error,
                ui.theme.alarm,
                Align::Left,
            );
        } else if searching {
            if visible_entries.is_empty() {
                ui.label(
                    entries_area.inset(ui.px(12)),
                    "No matches.",
                    ui.theme.text_dim,
                    Align::Left,
                );
            } else if action.is_none() {
                action = self.draw_entries(ui, entries_area, drawer, visible_entries, false);
            } else {
                self.draw_entries(ui, entries_area, drawer, visible_entries, false);
            }
        } else if let Some(path) = pending {
            ui.label(
                entries_area.inset(ui.px(12)),
                &format!("Loading {}…", path.display()),
                ui.theme.text_dim,
                Align::Left,
            );
        } else if let Some(listing) = listing {
            let drawn = if self.view == ViewMode::Thumbnails {
                self.draw_thumbnails(ui, entries_area, drawer, visible_entries, data.thumbnails)
            } else {
                self.draw_entries(ui, entries_area, drawer, visible_entries, listing.truncated)
            };
            if action.is_none() {
                action = drawn;
            }
        } else if current.is_none() {
            ui.label(
                entries_area.inset(ui.px(12)),
                "No local folder or mounted drive is available.",
                ui.theme.text_dim,
                Align::Left,
            );
        }

        if let Some(status) = data.status {
            let status_area = Rect::new(
                browser_area.x,
                browser_area.bottom() - ui.px(26),
                browser_area.w,
                ui.px(26),
            );
            ui.painter.fill_rect(status_area, ui.theme.panel_alt);
            ui.label(
                status_area.inset(ui.px(6)),
                status,
                ui.theme.text_dim,
                Align::Left,
            );
        }

        if action.is_none()
            && let Some(path) = current
            && listing.is_none_or(|listing| listing.directory != path)
            && pending.is_none()
            && error.is_none()
            && !searching
        {
            action = Some(FileBrowserAction::Browse {
                drawer,
                path: path.to_path_buf(),
                show_hidden: self.show_hidden,
            });
        }
        action
    }

    #[allow(clippy::too_many_lines)]
    fn handle_keys(
        &mut self,
        ui: &Ui<'_>,
        drawer: Drawer,
        sources: &BrowserSources,
        listing: Option<&DirectoryListing>,
        entries: &[BrowseEntry],
    ) -> Option<FileBrowserAction> {
        let mut action = None;
        let modifiers = ui.input.modifiers();
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        for key in ui.input.keys() {
            match key {
                KeyInput::Character('g') if modifiers.control => self.toggle_view(),
                KeyInput::Character('s') if modifiers.control => self.cycle_sort_field(entries),
                KeyInput::Character('r') if modifiers.control => {
                    self.toggle_sort_direction(entries);
                }
                KeyInput::Left => self.focus_area = FocusArea::Roots,
                KeyInput::Right => self.focus_area = FocusArea::Entries,
                KeyInput::Tab => {
                    self.focus_area = match self.focus_area {
                        FocusArea::Roots => FocusArea::Entries,
                        FocusArea::Entries => FocusArea::Roots,
                    };
                }
                KeyInput::Up => match self.focus_area {
                    FocusArea::Roots => self.root_focus = self.root_focus.saturating_sub(1),
                    FocusArea::Entries => {
                        let previous = self.entry_focus;
                        self.entry_focus = self.entry_focus.saturating_sub(1);
                        if let Some(path) = entries.get(self.entry_focus).map(|e| e.path.clone()) {
                            if modifiers.shift {
                                if self.entry_selection.is_empty()
                                    && let Some(anchor) = entries.get(previous)
                                {
                                    self.entry_selection.replace(anchor.path.clone());
                                }
                                self.entry_selection.extend_to(&order, path);
                            } else {
                                self.entry_selection.replace(path);
                            }
                        }
                    }
                },
                KeyInput::Down => match self.focus_area {
                    FocusArea::Roots => {
                        self.root_focus =
                            (self.root_focus + 1).min(sources.roots.len().saturating_sub(1));
                    }
                    FocusArea::Entries => {
                        let previous = self.entry_focus;
                        self.entry_focus =
                            (self.entry_focus + 1).min(entries.len().saturating_sub(1));
                        if let Some(path) = entries.get(self.entry_focus).map(|e| e.path.clone()) {
                            if modifiers.shift {
                                if self.entry_selection.is_empty()
                                    && let Some(anchor) = entries.get(previous)
                                {
                                    self.entry_selection.replace(anchor.path.clone());
                                }
                                self.entry_selection.extend_to(&order, path);
                            } else {
                                self.entry_selection.replace(path);
                            }
                        }
                    }
                },
                KeyInput::Home => match self.focus_area {
                    FocusArea::Roots => self.root_focus = 0,
                    FocusArea::Entries => self.entry_focus = 0,
                },
                KeyInput::End => match self.focus_area {
                    FocusArea::Roots => self.root_focus = sources.roots.len().saturating_sub(1),
                    FocusArea::Entries => {
                        self.entry_focus = entries.len().saturating_sub(1);
                    }
                },
                KeyInput::Backspace => {
                    if let Some(parent) = listing.and_then(|listing| listing.parent.as_ref()) {
                        action = Some(FileBrowserAction::Browse {
                            drawer,
                            path: parent.clone(),
                            show_hidden: self.show_hidden,
                        });
                    }
                }
                KeyInput::Character('a' | 'A') if modifiers.control => {
                    self.entry_selection.select_all(&order);
                    self.entry_focus = 0;
                    self.focus_area = FocusArea::Entries;
                }
                KeyInput::Enter => match self.focus_area {
                    FocusArea::Roots => {
                        if let Some(root) = sources.roots.get(self.root_focus) {
                            action = Some(FileBrowserAction::Browse {
                                drawer,
                                path: root.path.clone(),
                                show_hidden: self.show_hidden,
                            });
                        }
                    }
                    FocusArea::Entries => {
                        if let Some(entry) = entries.get(self.entry_focus) {
                            action = Some(action_for_entry(drawer, self.show_hidden, entry));
                        }
                    }
                },
                _ => {}
            }
        }
        action
    }

    fn draw_roots(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        drawer: Drawer,
        sources: &BrowserSources,
        current: Option<&Path>,
    ) -> Option<FileBrowserAction> {
        ui.painter.fill_rect(area, ui.theme.panel_alt);
        let (title, body) = area.inset(ui.px(6)).split_top(ui.px(30));
        ui.label_styled(
            title,
            "PLACES & DRIVES",
            TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
            ui.theme.text_dim,
            Align::Left,
        );
        let heights = vec![ui.px(ROOT_ROW_HEIGHT); sources.roots.len()];
        let mut selected = None;
        ui.scroll_list(&mut self.root_scroll, body, &heights, |ui, index, row| {
            let root = &sources.roots[index];
            let focused = self.focus_area == FocusArea::Roots && self.root_focus == index;
            let active = current.is_some_and(|path| path == root.path);
            if focused || active {
                ui.painter.fill_rect(row, ui.theme.selection);
            } else if ui.input.hovering(row) {
                ui.painter.fill_rect(row, ui.theme.hover);
            }
            let marker = match root.kind {
                RootKind::Place => "PLACE",
                RootKind::Drive => "DRIVE",
            };
            let (name_row, detail_row) = row.inset(ui.px(5)).split_top(ui.px(21));
            ui.label(
                name_row,
                &format!("{marker}  {}", root.label),
                ui.theme.text,
                Align::Left,
            );
            ui.label_styled(
                detail_row,
                &root.detail,
                TextStyle::new(ui.theme.body.face, 11.0),
                ui.theme.text_dim,
                Align::Left,
            );
            if ui.input.take_click(row) {
                self.focus_area = FocusArea::Roots;
                self.root_focus = index;
                selected = Some(root.path.clone());
            }
        });
        selected.map(|path| FileBrowserAction::Browse {
            drawer,
            path,
            show_hidden: self.show_hidden,
        })
    }

    fn draw_toolbar(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        drawer: Drawer,
        current: Option<&Path>,
        listing: Option<&DirectoryListing>,
        search_query: Option<&str>,
    ) -> Option<FileBrowserAction> {
        let (up, rest) = area.split_left(ui.px(64));
        let (refresh, rest) = rest.split_left(ui.px(88));
        let (hidden, rest) = rest.split_right(ui.px(116));
        let (view, rest) = rest.split_right(ui.px(96));
        let (direction, rest) = rest.split_right(ui.px(72));
        let (sort, path_area) = rest.split_right(ui.px(110));
        let entries: &[BrowseEntry] = listing.map_or(&[], |listing| listing.entries.as_slice());
        let sort_label = format!(
            "Sort: {}",
            match self.sort.field {
                SortField::NaturalName => "name",
                SortField::Kind => "type",
                SortField::Size => "size",
                SortField::Modified => "date",
                SortField::Manual => "manual",
            }
        );
        if ui.button(sort.inset(ui.px(4)), &sort_label) {
            self.cycle_sort_field(entries);
        }
        if ui.button(
            direction.inset(ui.px(4)),
            if self.sort.ascending { "Asc" } else { "Desc" },
        ) {
            self.toggle_sort_direction(entries);
        }
        if ui.button(
            view.inset(ui.px(4)),
            match self.view {
                ViewMode::List => "View: list",
                ViewMode::Thumbnails => "View: grid",
            },
        ) {
            self.toggle_view();
        }
        if ui.button(up.inset(ui.px(4)), "Up")
            && let Some(parent) = listing.and_then(|listing| listing.parent.as_ref())
        {
            return Some(FileBrowserAction::Browse {
                drawer,
                path: parent.clone(),
                show_hidden: self.show_hidden,
            });
        }
        if ui.button(refresh.inset(ui.px(4)), "Refresh")
            && let Some(path) = current
        {
            return Some(FileBrowserAction::Browse {
                drawer,
                path: path.to_path_buf(),
                show_hidden: self.show_hidden,
            });
        }
        if ui.button(
            hidden.inset(ui.px(4)),
            if self.show_hidden {
                "Hidden: on"
            } else {
                "Hidden: off"
            },
        ) {
            self.show_hidden = !self.show_hidden;
            if let Some(path) = current {
                return Some(FileBrowserAction::Browse {
                    drawer,
                    path: path.to_path_buf(),
                    show_hidden: self.show_hidden,
                });
            }
        }
        ui.label(
            path_area.inset(ui.px(8)),
            &search_query.map_or_else(
                || {
                    current.map_or_else(
                        || "No location".to_owned(),
                        |path| path.display().to_string(),
                    )
                },
                |query| format!("SEARCH RESULTS · {query}"),
            ),
            ui.theme.text,
            Align::Left,
        );
        None
    }

    fn draw_columns(ui: &mut Ui<'_>, area: Rect) {
        ui.painter.fill_rect(area, ui.theme.panel_alt);
        let (modified, rest) = area.split_right(ui.px(150).min(area.w / 3));
        let (size, name) = rest.split_right(ui.px(100).min(rest.w / 3));
        let style = TextStyle::tracked(ui.theme.body.face, 11.0, 0.08);
        ui.label_styled(
            name.inset(ui.px(7)),
            "NAME",
            style,
            ui.theme.text_dim,
            Align::Left,
        );
        ui.label_styled(
            size.inset(ui.px(7)),
            "SIZE",
            style,
            ui.theme.text_dim,
            Align::Right,
        );
        ui.label_styled(
            modified.inset(ui.px(7)),
            "MODIFIED",
            style,
            ui.theme.text_dim,
            Align::Right,
        );
    }

    /// Entries under the shared contract. The default descriptor is what the service already
    /// served, so this only runs for a non-default choice.
    fn sorted(entries: &[BrowseEntry], sort: SortDescriptor) -> Vec<BrowseEntry> {
        let mut ordered = entries.to_vec();
        ordered.sort_by(|a, b| compare(&Self::sort_item(a), &Self::sort_item(b), sort));
        ordered
    }

    fn sort_item(entry: &BrowseEntry) -> SortItem<'_> {
        SortItem {
            name: &entry.name,
            path: &entry.path,
            is_dir: entry.is_dir(),
            hidden: entry.hidden,
            kind_label: entry
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or(""),
            size_bytes: entry.size_bytes,
            mtime_ms: entry.mtime_ms,
            manual_index: None,
        }
    }

    pub fn sort(&self) -> SortDescriptor {
        self.sort
    }

    pub fn view(&self) -> ViewMode {
        self.view
    }

    /// Change the ordering. The path-keyed selection is untouched, and focus follows the entry
    /// it was on to its new row — `entries` is the order focus currently indexes into.
    pub fn set_sort(&mut self, sort: SortDescriptor, entries: &[BrowseEntry]) {
        let current = if self.sort == SortDescriptor::default() {
            entries.to_vec()
        } else {
            Self::sorted(entries, self.sort)
        };
        let focused = current
            .get(self.entry_focus)
            .map(|entry| entry.path.clone());
        self.sort = sort;
        if let Some(focused) = focused {
            let next = if sort == SortDescriptor::default() {
                entries.to_vec()
            } else {
                Self::sorted(entries, sort)
            };
            if let Some(index) = next.iter().position(|entry| entry.path == focused) {
                self.entry_focus = index;
            }
        }
    }

    /// The next primary sort field, in a fixed cycle — the toolbar button's action.
    pub fn cycle_sort_field(&mut self, entries: &[BrowseEntry]) {
        let field = match self.sort.field {
            SortField::NaturalName => SortField::Kind,
            SortField::Kind => SortField::Size,
            SortField::Size => SortField::Modified,
            SortField::Modified | SortField::Manual => SortField::NaturalName,
        };
        self.set_sort(SortDescriptor { field, ..self.sort }, entries);
    }

    pub fn toggle_sort_direction(&mut self, entries: &[BrowseEntry]) {
        self.set_sort(
            SortDescriptor {
                ascending: !self.sort.ascending,
                ..self.sort
            },
            entries,
        );
    }

    /// Switch between list and thumbnail presentation. A view is how the same rows are drawn:
    /// selection, focus and scroll context stay exactly as they are.
    pub fn set_view(&mut self, view: ViewMode) {
        self.view = view;
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            ViewMode::List => ViewMode::Thumbnails,
            ViewMode::Thumbnails => ViewMode::List,
        };
    }

    /// The most thumbnail decodes one call may put in flight.
    pub const THUMBNAIL_BATCH: usize = 16;

    /// Whether `generation` still names the current listing — the host drops a decoded
    /// thumbnail whose generation this refuses.
    pub fn thumbnail_current(&self, generation: u64) -> bool {
        generation == self.thumbnail_generation
    }

    /// Bounded thumbnail work for the host: only entries the last draw could see (plus its
    /// lookahead), only files, only ones not already decoded, and at most
    /// [`Self::THUMBNAIL_BATCH`] per call. Decoding happens in the host's async task — never
    /// here, never on the render thread.
    pub fn begin_thumbnails(
        &self,
        entries: &[BrowseEntry],
        have: &HashMap<PathBuf, Bitmap>,
    ) -> Vec<(u64, PathBuf)> {
        if self.view != ViewMode::Thumbnails {
            return Vec::new();
        }
        let ordered: Vec<BrowseEntry>;
        let entries: &[BrowseEntry] = if self.sort == SortDescriptor::default() {
            entries
        } else {
            ordered = Self::sorted(entries, self.sort);
            &ordered
        };
        let end = self.visible_thumbnails.end.min(entries.len());
        let start = self.visible_thumbnails.start.min(end);
        entries[start..end]
            .iter()
            .filter(|entry| !entry.is_dir())
            .filter(|entry| !have.contains_key(&entry.path))
            .take(Self::THUMBNAIL_BATCH)
            .map(|entry| (self.thumbnail_generation, entry.path.clone()))
            .collect()
    }

    /// The thumbnail grid: the same entries, selection and focus as the list, drawn as
    /// responsive preview cells. Cells without a decoded bitmap show the entry's kind chip; the
    /// grid itself never reads a file.
    #[allow(clippy::too_many_lines)]
    fn draw_thumbnails(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        drawer: Drawer,
        entries: &[BrowseEntry],
        thumbnails: &HashMap<PathBuf, Bitmap>,
    ) -> Option<FileBrowserAction> {
        if entries.is_empty() {
            ui.label(
                area.inset(ui.px(12)),
                "This folder is empty.",
                ui.theme.text_dim,
                Align::Left,
            );
            return None;
        }
        let cell = ui.px(120).max(40);
        let columns = ((area.w / cell).max(1)) as usize;
        let rows = entries.len().div_ceil(columns);
        let heights = vec![cell; rows];
        let mut chosen = None;
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        let mut seen: Option<(usize, usize)> = None;
        ui.scroll_list(
            &mut self.entry_scroll,
            area,
            &heights,
            |ui, row_index, row| {
                for slot in 0..columns {
                    let index = row_index * columns + slot;
                    let Some(entry) = entries.get(index) else {
                        break;
                    };
                    seen = Some(match seen {
                        None => (index, index + 1),
                        Some((low, high)) => (low.min(index), high.max(index + 1)),
                    });
                    let slot_i = i32::try_from(slot).unwrap_or(0);
                    let tile = Rect::new(row.x + slot_i * cell, row.y, cell, row.h).inset(ui.px(4));
                    let focused =
                        self.focus_area == FocusArea::Entries && self.entry_focus == index;
                    let in_selection = self.entry_selection.contains(&entry.path);
                    if focused || in_selection {
                        ui.painter.fill_rect(tile, ui.theme.selection);
                    } else if ui.input.hovering(tile) {
                        ui.painter.fill_rect(tile, ui.theme.hover);
                    } else {
                        ui.painter.fill_rect(tile, ui.theme.panel_alt);
                    }
                    let (image, caption) = tile.split_bottom(ui.px(18));
                    if let Some(bitmap) = thumbnails.get(&entry.path) {
                        ui.painter.blit(
                            bitmap,
                            image.inset(ui.px(2)),
                            pixelkit_raster::ScaleFilter::Bilinear,
                        );
                    } else {
                        let chip = entry
                            .path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .map_or_else(
                                || if entry.is_dir() { "FOLDER" } else { "FILE" }.to_owned(),
                                str::to_uppercase,
                            );
                        ui.label(image, &chip, ui.theme.text_dim, Align::Centre);
                    }
                    ui.label(
                        caption.inset(ui.px(2)),
                        &entry.name,
                        ui.theme.text,
                        Align::Centre,
                    );
                    if ui.input.take_click(tile) {
                        self.focus_area = FocusArea::Entries;
                        let previous = self.entry_focus;
                        self.entry_focus = index;
                        let modifiers = ui.input.modifiers();
                        if modifiers.control {
                            self.entry_selection.toggle(entry.path.clone());
                        } else if modifiers.shift {
                            if self.entry_selection.is_empty()
                                && let Some(anchor) = entries.get(previous)
                            {
                                self.entry_selection.replace(anchor.path.clone());
                            }
                            self.entry_selection.extend_to(&order, entry.path.clone());
                        } else {
                            self.entry_selection.replace(entry.path.clone());
                            chosen = Some(entry.clone());
                        }
                    }
                }
            },
        );
        if let Some((low, high)) = seen {
            // One row of lookahead in each direction, so a slow scroll never shows a beat of
            // empty cells.
            self.visible_thumbnails =
                low.saturating_sub(columns)..(high + columns).min(entries.len());
        }
        chosen.map(|entry| action_for_entry(drawer, self.show_hidden, &entry))
    }

    fn draw_entries(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        drawer: Drawer,
        entries: &[BrowseEntry],
        truncated: bool,
    ) -> Option<FileBrowserAction> {
        if entries.is_empty() {
            ui.label(
                area.inset(ui.px(12)),
                "This folder is empty.",
                ui.theme.text_dim,
                Align::Left,
            );
            return None;
        }
        let heights = vec![ui.px(ENTRY_HEIGHT); entries.len()];
        let mut selected = None;
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        ui.scroll_list(&mut self.entry_scroll, area, &heights, |ui, index, row| {
            let entry = &entries[index];
            let focused = self.focus_area == FocusArea::Entries && self.entry_focus == index;
            let in_selection = self.entry_selection.contains(&entry.path);
            if focused || in_selection {
                ui.painter.fill_rect(row, ui.theme.selection);
            } else if ui.input.hovering(row) {
                ui.painter.fill_rect(row, ui.theme.hover);
            } else if index % 2 == 1 {
                ui.painter.fill_rect(row, ui.theme.panel_alt);
            }
            let (modified, rest) = row.split_right(ui.px(150).min(row.w / 3));
            let (size, name) = rest.split_right(ui.px(100).min(rest.w / 3));
            let kind = match entry.kind {
                EntryKind::Directory => "FOLDER",
                EntryKind::File => "FILE",
                EntryKind::Other => "OTHER",
            };
            let link = if entry.is_symlink { " · LINK" } else { "" };
            ui.label(
                name.inset(ui.px(7)),
                &format!("{kind}{link}   {}", entry.name),
                ui.theme.text,
                Align::Left,
            );
            let size_label = if entry.is_dir() {
                "—".to_owned()
            } else {
                crate::format::bytes(entry.size_bytes)
            };
            ui.label(
                size.inset(ui.px(7)),
                &size_label,
                ui.theme.text_dim,
                Align::Right,
            );
            ui.label_styled(
                modified.inset(ui.px(7)),
                &crate::format::timestamp_ms(entry.mtime_ms),
                TextStyle::new(ui.theme.body.face, 12.0),
                ui.theme.text_dim,
                Align::Right,
            );
            if ui.input.take_click(row) {
                self.focus_area = FocusArea::Entries;
                let previous = self.entry_focus;
                self.entry_focus = index;
                let modifiers = ui.input.modifiers();
                if modifiers.control {
                    self.entry_selection.toggle(entry.path.clone());
                } else if modifiers.shift {
                    if self.entry_selection.is_empty()
                        && let Some(anchor) = entries.get(previous)
                    {
                        self.entry_selection.replace(anchor.path.clone());
                    }
                    self.entry_selection.extend_to(&order, entry.path.clone());
                } else {
                    self.entry_selection.replace(entry.path.clone());
                    selected = Some(entry.clone());
                }
            }
        });
        if truncated {
            let note = Rect::new(area.x, area.bottom() - ui.px(24), area.w, ui.px(24));
            ui.painter.fill_rect(note, ui.theme.panel_alt);
            ui.label(
                note.inset(ui.px(5)),
                "More entries exist; narrow the folder with Search.",
                ui.theme.text_dim,
                Align::Right,
            );
        }
        selected.map(|entry| action_for_entry(drawer, self.show_hidden, &entry))
    }
}

fn action_for_entry(drawer: Drawer, show_hidden: bool, entry: &BrowseEntry) -> FileBrowserAction {
    if entry.is_dir() {
        FileBrowserAction::Browse {
            drawer,
            path: entry.path.clone(),
            show_hidden,
        }
    } else {
        FileBrowserAction::Open(entry.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_browser::BrowserRoot;
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, Modifiers, MouseButton, Scale};

    struct Harness {
        screen: FileBrowserScreen,
        sources: BrowserSources,
        listing: DirectoryListing,
        buffer: WindowBuffer,
        text: pixelkit_text::TextCache,
        input: Input,
        kernel: RasterKernel,
    }

    impl Harness {
        fn new() -> Self {
            let root = PathBuf::from("/home/test/Downloads");
            Self {
                screen: FileBrowserScreen::default(),
                sources: BrowserSources {
                    roots: vec![BrowserRoot {
                        label: "Downloads".into(),
                        path: root.clone(),
                        detail: "Standard folder".into(),
                        kind: RootKind::Place,
                    }],
                    starts: [(Drawer::Downloads, root.clone())].into(),
                },
                listing: DirectoryListing {
                    directory: root.clone(),
                    parent: Some(PathBuf::from("/home/test")),
                    entries: vec![
                        BrowseEntry {
                            path: root.join("Folder"),
                            name: "Folder".into(),
                            kind: EntryKind::Directory,
                            is_symlink: false,
                            hidden: false,
                            size_bytes: 0,
                            mtime_ms: 0,
                        },
                        BrowseEntry {
                            path: root.join("paper.pdf"),
                            name: "paper.pdf".into(),
                            kind: EntryKind::File,
                            is_symlink: false,
                            hidden: false,
                            size_bytes: 1024,
                            mtime_ms: 0,
                        },
                    ],
                    truncated: false,
                    limit: 500,
                },
                buffer: WindowBuffer::new(1000, 600),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                input: Input::new(),
                kernel: RasterKernel::new(),
            }
        }

        fn frame_with_search(
            &mut self,
            search_query: Option<&str>,
            search_entries: Option<&[BrowseEntry]>,
        ) -> Option<FileBrowserAction> {
            let empty_thumbnails = std::collections::HashMap::new();
            let data = FileBrowserData {
                sources: &self.sources,
                listing: Some((Drawer::Downloads, false, &self.listing)),
                pending: None,
                error: None,
                status: None,
                search_query,
                search_entries,
                search_error: None,
                thumbnails: &empty_thumbnails,
            };
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                crate::surface::design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let action = self.screen.draw(
                &mut ui,
                Rect::new(0, 0, 1000, 600),
                Drawer::Downloads,
                &data,
                true,
            );
            self.input.end_frame();
            action
        }

        fn frame(&mut self) -> Option<FileBrowserAction> {
            self.frame_with_search(None, None)
        }

        fn click(&mut self, x: f32, y: f32) -> Option<FileBrowserAction> {
            self.input.cursor_moved(x, y);
            self.input.mouse(MouseButton::Left, true);
            self.input.mouse(MouseButton::Left, false);
            self.frame()
        }
    }

    #[test]
    fn file_browser_pointer_activates_folders_and_files() {
        let mut harness = Harness::new();
        harness.frame();
        // Browser begins after the 220 px roots rail; entries begin below the 42 + 28 px bars.
        assert_eq!(
            harness.click(400.0, 88.0),
            Some(FileBrowserAction::Browse {
                drawer: Drawer::Downloads,
                path: PathBuf::from("/home/test/Downloads/Folder"),
                show_hidden: false,
            })
        );
        assert_eq!(
            harness.click(400.0, 126.0),
            Some(FileBrowserAction::Open(PathBuf::from(
                "/home/test/Downloads/paper.pdf"
            )))
        );
    }

    #[test]
    fn file_browser_keyboard_navigates_entries_roots_and_parent() {
        let mut harness = Harness::new();
        harness.input.key(KeyInput::Enter);
        assert!(matches!(
            harness.frame(),
            Some(FileBrowserAction::Browse { path, .. }) if path.ends_with("Folder")
        ));

        harness.input.key(KeyInput::Down);
        harness.input.key(KeyInput::Enter);
        assert!(matches!(
            harness.frame(),
            Some(FileBrowserAction::Open(path)) if path.ends_with("paper.pdf")
        ));

        harness.input.key(KeyInput::Backspace);
        assert_eq!(
            harness.frame(),
            Some(FileBrowserAction::Browse {
                drawer: Drawer::Downloads,
                path: PathBuf::from("/home/test"),
                show_hidden: false,
            })
        );

        harness.input.key(KeyInput::Left);
        harness.input.key(KeyInput::Enter);
        assert!(matches!(
            harness.frame(),
            Some(FileBrowserAction::Browse { path, .. })
                if path == Path::new("/home/test/Downloads")
        ));
    }

    #[test]
    fn file_browser_shift_and_control_select_without_opening() {
        let mut harness = Harness::new();
        harness.frame();

        harness.input.modifiers_changed(Modifiers {
            shift: true,
            ..Modifiers::default()
        });
        harness.input.key(KeyInput::Down);
        assert_eq!(harness.frame(), None);
        assert_eq!(
            harness.screen.selected_paths(),
            vec![
                PathBuf::from("/home/test/Downloads/Folder"),
                PathBuf::from("/home/test/Downloads/paper.pdf"),
            ]
        );

        harness.input.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        harness.input.cursor_moved(400.0, 88.0);
        harness.input.mouse(MouseButton::Left, true);
        harness.input.mouse(MouseButton::Left, false);
        assert_eq!(harness.frame(), None);
        assert_eq!(
            harness.screen.selected_paths(),
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
        );

        harness.input.key(KeyInput::Character('a'));
        assert_eq!(harness.frame(), None);
        assert_eq!(harness.screen.selected_paths().len(), 2);
    }

    #[test]
    fn file_browser_search_results_use_the_same_clickable_rows() {
        let mut harness = Harness::new();
        let entries = harness.listing.entries.clone();
        harness.frame_with_search(Some("paper"), Some(&entries));
        harness.input.cursor_moved(400.0, 126.0);
        harness.input.mouse(MouseButton::Left, true);
        harness.input.mouse(MouseButton::Left, false);
        assert_eq!(
            harness.frame_with_search(Some("paper"), Some(&entries)),
            Some(FileBrowserAction::Open(PathBuf::from(
                "/home/test/Downloads/paper.pdf"
            )))
        );
    }

    /// Switching between the list and the thumbnail grid preserves selection, focus and the
    /// scroll state — a view is how the same rows are drawn, nothing more.
    #[test]
    fn view_mode_switch_preserves_selection_focus_and_scroll() {
        let mut harness = Harness::new();
        harness.frame(); // settle the directory
        harness.screen.entry_focus = 1;
        harness
            .screen
            .entry_selection
            .replace(harness.listing.entries[1].path.clone());
        let selected_before = harness.screen.selected_paths();
        let focus_before = harness.screen.entry_focus;

        harness.screen.toggle_view();
        assert_eq!(harness.screen.view(), ViewMode::Thumbnails);
        harness.frame();
        assert_eq!(harness.screen.selected_paths(), selected_before);
        assert_eq!(harness.screen.entry_focus, focus_before);

        harness.screen.toggle_view();
        assert_eq!(harness.screen.view(), ViewMode::List);
        harness.frame();
        assert_eq!(harness.screen.selected_paths(), selected_before);
        assert_eq!(harness.screen.entry_focus, focus_before);
    }

    /// Re-sorting uses the shared contract, keeps the path-keyed selection, and focus follows
    /// its entry to the new row.
    #[test]
    fn view_mode_sort_keeps_selection_and_focus_follows_its_entry() {
        let mut harness = Harness::new();
        harness.frame();
        // Focus the file (index 1 in default folder-first order), select it.
        harness.screen.entry_focus = 1;
        let file = harness.listing.entries[1].path.clone();
        harness.screen.entry_selection.replace(file.clone());

        harness.screen.set_sort(
            SortDescriptor {
                field: SortField::NaturalName,
                ascending: false,
                folders_first: false,
            },
            &harness.listing.entries,
        );
        // Descending, folders no longer first: paper.pdf now leads.
        let ordered = FileBrowserScreen::sorted(&harness.listing.entries, harness.screen.sort());
        assert_eq!(ordered[0].path, file);
        assert_eq!(harness.screen.entry_focus, 0, "focus followed paper.pdf");
        assert_eq!(harness.screen.selected_paths(), vec![file]);
    }

    /// The grid asks for thumbnails boundedly: only what the draw could see plus a row of
    /// lookahead, never a directory, never one it already has, and at most one batch — and a
    /// directory change makes every outstanding request recognisably stale.
    #[test]
    fn thumbnail_requests_are_bounded_deduplicated_and_stale_after_navigation() {
        let mut harness = Harness::new();
        // Many files so the visibility bound matters.
        let root = harness.listing.directory.clone();
        harness.listing.entries = (0..60)
            .map(|index| BrowseEntry {
                path: root.join(format!("photo{index:02}.png")),
                name: format!("photo{index:02}.png"),
                kind: EntryKind::File,
                is_symlink: false,
                hidden: false,
                size_bytes: 1,
                mtime_ms: 0,
            })
            .collect();
        let have = std::collections::HashMap::new();

        assert!(
            harness
                .screen
                .begin_thumbnails(&harness.listing.entries, &have)
                .is_empty(),
            "list mode requests nothing"
        );
        harness.screen.set_view(ViewMode::Thumbnails);
        harness.frame();
        let requests = harness
            .screen
            .begin_thumbnails(&harness.listing.entries, &have);
        assert!(!requests.is_empty(), "the visible cells are asked for");
        assert!(
            requests.len() <= FileBrowserScreen::THUMBNAIL_BATCH,
            "one bounded batch, however long the directory: {}",
            requests.len()
        );
        let generation = requests[0].0;
        assert!(harness.screen.thumbnail_current(generation));

        let mut have = std::collections::HashMap::new();
        have.insert(requests[0].1.clone(), Bitmap::new(1, 1));
        assert!(
            !harness
                .screen
                .begin_thumbnails(&harness.listing.entries, &have)
                .iter()
                .any(|(_, path)| *path == requests[0].1),
            "a decoded thumbnail is not requested again"
        );

        // Navigating renders the old generation stale: a slow decode cannot land in the new
        // directory.
        harness.listing.directory = PathBuf::from("/home/test/Other");
        harness.listing.entries.clear();
        harness.frame();
        assert!(
            !harness.screen.thumbnail_current(generation),
            "outstanding requests are from a dead generation"
        );
    }
}
