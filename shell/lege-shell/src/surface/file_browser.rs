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

/// The ordinary file verbs, named once so the keyboard and the pointer reach the same thing.
///
/// One vocabulary rather than two code paths: every accelerator below and every toolbar button
/// resolves to one of these, and the host performs it through the same `lege-filed` interfaces
/// the Files app uses. That is what "the same service semantics" means in practice — not similar
/// code in two places, but one request shape both surfaces send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileVerb {
    Copy,
    Cut,
    Paste,
    /// To the trash, through the journalled service. Never an unlink.
    Trash,
    Rename,
    NewFolder,
    Properties,
    CopyPath,
}

/// One button on the file-verb row: the declared action it performs, and the verb that performs
/// it.
///
/// A named type rather than a tuple so that this row is discoverable as a *pointer route*:
/// `cargo xtask check-actions` reads this constructor the same way it reads a widget region or a
/// menu row, which is what stops a file verb existing on a keyboard and nowhere else.
/// The project's rule is that a chord is an accelerator and never the only route; this is the
/// table that keeps it true here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileVerbRoute {
    pub action: &'static str,
    pub verb: FileVerb,
}

impl FileVerbRoute {
    pub const fn new(action: &'static str, verb: FileVerb) -> FileVerbRoute {
        FileVerbRoute { action, verb }
    }
}

/// Every file verb the Home file surfaces offer, in the order the row shows them.
///
/// One table, read by the row that draws the buttons and by [`FileVerb::action_id`], so a verb
/// cannot acquire a second identity by being spelled differently in two places.
pub const FILE_VERB_ROUTES: &[FileVerbRoute] = &[
    FileVerbRoute::new("files.copy", FileVerb::Copy),
    FileVerbRoute::new("files.cut", FileVerb::Cut),
    FileVerbRoute::new("files.paste", FileVerb::Paste),
    FileVerbRoute::new("files.rename", FileVerb::Rename),
    FileVerbRoute::new("files.new-folder", FileVerb::NewFolder),
    FileVerbRoute::new("files.trash", FileVerb::Trash),
    FileVerbRoute::new("files.copy-path", FileVerb::CopyPath),
    FileVerbRoute::new("files.properties", FileVerb::Properties),
];

impl FileVerb {
    /// The action id in `lege_types::action::MANIFEST` this verb performs.
    pub fn action_id(self) -> &'static str {
        FILE_VERB_ROUTES
            .iter()
            .find(|route| route.verb == self)
            .map_or("files.unknown", |route| route.action)
    }

    /// Whether this verb acts on the selection. The rest act on the folder being shown, and
    /// offering them with nothing selected is correct rather than a mistake.
    pub fn needs_selection(self) -> bool {
        !matches!(self, Self::Paste | Self::NewFolder)
    }

    /// What a button or a menu row calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::Trash => "Trash",
            Self::Rename => "Rename",
            Self::NewFolder => "New folder",
            Self::Properties => "Properties",
            Self::CopyPath => "Copy path",
        }
    }
}

/// One file verb, with everything the host needs to perform it and nothing it would have to
/// re-derive: the surface knows what is selected and which folder is open, so it says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRequest {
    pub verb: FileVerb,
    /// The selection, in the order the listing shows it. Empty for a verb that does not need one.
    pub paths: Vec<PathBuf>,
    /// The folder currently being shown — a paste destination, a new folder's parent.
    pub directory: Option<PathBuf>,
    /// The name a rename was given, once the person has typed one. `None` for every other verb.
    pub new_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileBrowserAction {
    Browse {
        drawer: Drawer,
        path: PathBuf,
        show_hidden: bool,
    },
    Open(PathBuf),
    /// An ordinary file verb, for the host to perform through `lege-filed`.
    File(FileRequest),
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
    /// The open rename dialogue, if any.
    rename: Option<RenameDialog>,
}

/// Renaming one item: which one, what the person has typed, and why it is refused so far.
///
/// A dialogue rather than an inline edit because the listing re-sorts as you type — a file being
/// renamed from "a" to "z" would walk out from under the caret — and because the refusal has
/// somewhere to be said.
#[derive(Debug)]
pub struct RenameDialog {
    path: PathBuf,
    field: pixelkit_ui::TextFieldState,
    /// Why the name typed so far cannot be used. Shown live, so the Rename button being inert is
    /// explained rather than mysterious.
    error: Option<&'static str>,
}

impl RenameDialog {
    fn new(path: PathBuf) -> RenameDialog {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        RenameDialog {
            path,
            field: pixelkit_ui::TextFieldState::with_text(name),
            error: None,
        }
    }

    /// The name as typed, or why it cannot be used.
    ///
    /// The same rule the service enforces (`lege_client::files::valid_file_name`), checked here
    /// only so the person is told *before* pressing Rename. The service still refuses
    /// independently — this is feedback, not the authority.
    fn validated(&self) -> Result<String, &'static str> {
        let typed = self.field.text().trim();
        if typed.is_empty() {
            return Err("A name is required.");
        }
        if !lege_client::files::valid_file_name(typed) {
            return Err("Use one filename, without / . or ..");
        }
        if self
            .path
            .file_name()
            .is_some_and(|current| current == std::ffi::OsStr::new(typed))
        {
            return Err("That is already its name.");
        }
        Ok(typed.to_owned())
    }
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
            rename: None,
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

        // The rename dialogue owns the frame while it is open: it takes the keys, so nothing
        // underneath can act on a keystroke meant for the name being typed.
        let renaming = self.rename.is_some();
        let mut action = if keyboard_enabled && !renaming {
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
        let (verbs, below_verbs) = below_toolbar.split_top(ui.px(TOOLBAR_HEIGHT));
        let verb_action = self.draw_file_verbs(ui, verbs, listing);
        if action.is_none() {
            action = verb_action;
        }
        let (columns, entries_area) = below_verbs.split_top(ui.px(COLUMNS_HEIGHT));
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

        // Drawn last so it sits over the listing, and consulted before the automatic browse
        // below: a dialogue waiting for an answer must not be reloaded out from under itself.
        let directory = listing.map(|listing| listing.directory.as_path());
        if let Some(renamed) = self.draw_rename(ui, entries_area, directory) {
            return Some(renamed);
        }
        if self.rename.is_some() {
            return action;
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
    /// Turn a verb into a request, or refuse it because there is nothing for it to act on.
    ///
    /// The single gate every route goes through, so a toolbar button and an accelerator cannot
    /// disagree about when a verb applies — and so "Trash with nothing selected" is a no-op here
    /// rather than a request the service has to refuse.
    /// Perform `verb`. Has side effects — Rename opens its dialogue — so it is only ever called
    /// for a verb somebody actually asked for.
    ///
    /// Kept apart from [`Self::can_request`] because the two were briefly the same function, and
    /// the verb row calls the availability check once per button per frame to decide what to grey
    /// out. A query that performs the action is a query that opened the rename dialogue sixty
    /// times a second and swallowed every keystroke aimed at the listing underneath it.
    fn request(&mut self, verb: FileVerb, directory: Option<&Path>) -> Option<FileBrowserAction> {
        if verb == FileVerb::Rename {
            let selected = self.entry_selection.to_vec();
            let [only] = selected.as_slice() else {
                return None;
            };
            // Opening the dialogue *is* performing Rename; the request comes with the name.
            self.rename = Some(RenameDialog::new(only.clone()));
            return None;
        }
        self.can_request(verb, directory)
    }

    /// Whether `verb` applies right now, and the request it would make. No side effects.
    fn can_request(&self, verb: FileVerb, directory: Option<&Path>) -> Option<FileBrowserAction> {
        let paths = self.entry_selection.to_vec();
        if verb.needs_selection() && paths.is_empty() {
            return None;
        }
        // Rename and Properties are about one item; asking for either with a dozen selected has
        // no single answer, so the surface does not ask.
        if matches!(
            verb,
            FileVerb::Rename | FileVerb::Properties | FileVerb::CopyPath
        ) && paths.len() != 1
        {
            return None;
        }
        if matches!(verb, FileVerb::Paste | FileVerb::NewFolder) && directory.is_none() {
            return None;
        }
        Some(FileBrowserAction::File(FileRequest {
            verb,
            paths,
            directory: directory.map(Path::to_path_buf),
            new_name: None,
        }))
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
        let directory = listing.map(|listing| listing.directory.as_path());
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        for key in ui.input.keys() {
            match key {
                // The ordinary file verbs. Surface-local accelerators, exactly like Ctrl+G and
                // Ctrl+S beneath them: these are not compositor-reserved chords, so they cost
                // the recovery keys nothing and work only where a file surface has focus.
                KeyInput::Character('c' | 'C') if modifiers.control && modifiers.shift => {
                    action = action.or_else(|| self.request(FileVerb::CopyPath, directory));
                }
                KeyInput::Character('c' | 'C') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Copy, directory));
                }
                KeyInput::Character('x' | 'X') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Cut, directory));
                }
                KeyInput::Character('v' | 'V') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Paste, directory));
                }
                KeyInput::Character('n' | 'N') if modifiers.control && modifiers.shift => {
                    action = action.or_else(|| self.request(FileVerb::NewFolder, directory));
                }
                KeyInput::Delete => {
                    action = action.or_else(|| self.request(FileVerb::Trash, directory));
                }
                KeyInput::Function(2) => {
                    action = action.or_else(|| self.request(FileVerb::Rename, directory));
                }
                KeyInput::Enter if modifiers.alt => {
                    action = action.or_else(|| self.request(FileVerb::Properties, directory));
                }
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
                KeyInput::Enter if !modifiers.alt => match self.focus_area {
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

    /// The rename dialogue: a name field, a refusal, and two buttons.
    ///
    /// Returns the confirmed request, and consumes the frame's keys while open so a stray Delete
    /// aimed at the field cannot trash the item being renamed.
    fn draw_rename(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        directory: Option<&Path>,
    ) -> Option<FileBrowserAction> {
        let dialog = self.rename.as_mut()?;
        let keys = ui.input.take_keys();
        if keys.contains(&KeyInput::Escape) {
            self.rename = None;
            return None;
        }
        let confirmed = keys.contains(&KeyInput::Enter);
        dialog.field.apply(&keys);
        dialog.error = dialog.validated().err();

        // A panel over the listing rather than beside it: the item is not going anywhere and the
        // question is the only thing to answer.
        let panel = Rect::new(
            area.x + area.w / 2 - ui.px(210),
            area.y + ui.px(40),
            ui.px(420).min(area.w),
            ui.px(150),
        );
        ui.painter.fill_rect(panel, ui.theme.panel);
        ui.painter
            .stroke_rect(panel, ui.scale.hairline(), ui.theme.panel_edge);
        let inner = panel.inset(ui.px(14));
        let (title, rest) = inner.split_top(ui.px(24));
        ui.label(
            title,
            &format!(
                "Rename {}",
                dialog.path.file_name().map_or_else(
                    || dialog.path.display().to_string(),
                    |name| name.to_string_lossy().into_owned()
                )
            ),
            ui.theme.text,
            Align::Left,
        );
        let (field, rest) = rest.split_top(ui.px(34));
        ui.text_field(
            &mut dialog.field,
            field.inset(ui.px(3)),
            "New name",
            true,
            false,
        );
        let (message, buttons) = rest.split_top(ui.px(24));
        if let Some(error) = dialog.error {
            ui.label(message, error, ui.theme.alarm, Align::Left);
        }

        let (cancel, rest) = buttons.split_right(ui.px(96));
        let (rename, _) = rest.split_right(ui.px(96));
        let valid = dialog.validated().ok();
        if ui.button(cancel.inset(ui.px(4)), "Cancel") {
            self.rename = None;
            return None;
        }
        let pressed = if let Some(new_name) = valid.clone() {
            let clicked = ui.button(rename.inset(ui.px(4)), "Rename");
            (clicked || confirmed).then_some(new_name)
        } else {
            // Inert rather than absent, with the reason above it: a button that vanishes as you
            // type is a button you cannot learn.
            ui.painter
                .fill_rounded_rect(rename.inset(ui.px(4)), ui.px(4), ui.theme.panel_alt);
            ui.label(
                rename.inset(ui.px(4)),
                "Rename",
                ui.theme.text_dim,
                Align::Centre,
            );
            None
        };
        let new_name = pressed?;
        let path = dialog.path.clone();
        self.rename = None;
        Some(FileBrowserAction::File(FileRequest {
            verb: FileVerb::Rename,
            paths: vec![path],
            directory: directory.map(Path::to_path_buf),
            new_name: Some(new_name),
        }))
    }

    /// The file verbs as buttons: every action a keyboard can reach here, reachable by pointer.
    ///
    /// A keyboard-only capability is a capability half the people using this machine do not
    /// have, and the project's own rule is that a chord is an accelerator and never the only
    /// route. A button whose verb does not currently apply is drawn disabled rather than hidden,
    /// so the row does not reshuffle under the pointer as the selection changes.
    fn draw_file_verbs(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        listing: Option<&DirectoryListing>,
    ) -> Option<FileBrowserAction> {
        let directory = listing.map(|listing| listing.directory.as_path());
        let mut cursor = area;
        let mut pressed: Option<FileVerb> = None;
        for verb in FILE_VERB_ROUTES.iter().map(|route| route.verb) {
            let width = ui.px(
                if matches!(verb, FileVerb::NewFolder | FileVerb::CopyPath) {
                    104
                } else {
                    88
                },
            );
            if cursor.w < width {
                break;
            }
            let (button, rest) = cursor.split_left(width);
            cursor = rest;
            // The availability query, never the performing one: this runs for every button on
            // every frame.
            let enabled = self.can_request(verb, directory).is_some();
            if enabled {
                if ui.button(button.inset(ui.px(4)), verb.label()) {
                    pressed = Some(verb);
                }
            } else {
                // Present but visibly inert: the control does not move, and its absence is not
                // something a person has to go looking for.
                ui.painter
                    .fill_rounded_rect(button.inset(ui.px(4)), ui.px(4), ui.theme.panel_alt);
                ui.label(
                    button.inset(ui.px(4)),
                    verb.label(),
                    ui.theme.text_dim,
                    Align::Centre,
                );
            }
        }
        // Performed after the loop, so the one verb with a side effect has one chance to run.
        self.request(pressed?, directory)
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

    impl Harness {
        /// Press a key with modifiers and report what the surface asked for.
        fn press(&mut self, key: KeyInput, modifiers: Modifiers) -> Option<FileBrowserAction> {
            self.input.modifiers_changed(modifiers);
            self.input.key(key);
            let action = self.frame();
            self.input.modifiers_changed(Modifiers::default());
            action
        }

        fn select(&mut self, paths: &[&str]) {
            self.screen.entry_selection.clear();
            for path in paths {
                self.screen.entry_selection.toggle(PathBuf::from(path));
            }
        }
    }

    fn control() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    fn control_shift() -> Modifiers {
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        }
    }

    fn verb_of(action: Option<FileBrowserAction>) -> Option<(FileVerb, Vec<PathBuf>)> {
        match action {
            Some(FileBrowserAction::File(request)) => Some((request.verb, request.paths)),
            _ => None,
        }
    }

    #[test]
    fn file_actions_are_reachable_by_keyboard_over_the_selection() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&["/home/test/Downloads/paper.pdf"]);
        let one = vec![PathBuf::from("/home/test/Downloads/paper.pdf")];

        for (key, modifiers, verb) in [
            (KeyInput::Character('c'), control(), FileVerb::Copy),
            (KeyInput::Character('x'), control(), FileVerb::Cut),
            (KeyInput::Character('v'), control(), FileVerb::Paste),
            (KeyInput::Delete, Modifiers::default(), FileVerb::Trash),
            (
                KeyInput::Character('c'),
                control_shift(),
                FileVerb::CopyPath,
            ),
        ] {
            assert_eq!(
                verb_of(harness.press(key, modifiers)),
                Some((verb, one.clone())),
                "{verb:?} has no keyboard route"
            );
        }
        // Alt+Enter is Properties and must not also open the focused entry.
        assert_eq!(
            verb_of(harness.press(
                KeyInput::Enter,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                }
            )),
            Some((FileVerb::Properties, one))
        );
        // New folder acts on the open directory, so it needs no selection at all.
        harness.select(&[]);
        let request = match harness.press(KeyInput::Character('n'), control_shift()) {
            Some(FileBrowserAction::File(request)) => request,
            other => panic!("no new-folder request: {other:?}"),
        };
        assert_eq!(request.verb, FileVerb::NewFolder);
        assert_eq!(
            request.directory,
            Some(PathBuf::from("/home/test/Downloads"))
        );
    }

    #[test]
    fn file_actions_rename_asks_for_a_name_and_only_then_becomes_a_request() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&["/home/test/Downloads/paper.pdf"]);

        // F2 opens the dialogue rather than emitting a request: a rename with no name is not a
        // request anybody made.
        assert_eq!(
            verb_of(harness.press(KeyInput::Function(2), Modifiers::default())),
            None
        );
        assert!(harness.screen.rename.is_some(), "F2 opened no dialogue");
        // Pre-filled with the current name, which is what a person edits rather than retypes.
        assert_eq!(
            harness.screen.rename.as_ref().unwrap().field.text(),
            "paper.pdf"
        );

        // The dialogue owns the keys while it is open, so a stray Delete aimed at the field
        // cannot trash the very item being renamed.
        assert_eq!(
            verb_of(harness.press(KeyInput::Delete, Modifiers::default())),
            None
        );
        assert!(harness.screen.rename.is_some());

        // Type a new name and confirm it.
        harness
            .screen
            .rename
            .as_mut()
            .unwrap()
            .field
            .set_text("report.pdf");
        let action = harness.press(KeyInput::Enter, Modifiers::default());
        let Some(FileBrowserAction::File(request)) = action else {
            panic!("Enter did not confirm the rename: {action:?}");
        };
        assert_eq!(request.verb, FileVerb::Rename);
        assert_eq!(
            request.paths,
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
        );
        assert_eq!(request.new_name.as_deref(), Some("report.pdf"));
        assert!(harness.screen.rename.is_none(), "the dialogue stayed open");
    }

    #[test]
    fn file_actions_rename_refuses_a_name_that_is_not_one_and_says_why() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&["/home/test/Downloads/paper.pdf"]);
        harness.press(KeyInput::Function(2), Modifiers::default());

        for (typed, reason) in [
            ("", "A name is required."),
            ("   ", "A name is required."),
            ("../escape", "Use one filename, without / . or .."),
            ("sub/deeper", "Use one filename, without / . or .."),
            ("..", "Use one filename, without / . or .."),
            // Renaming something to what it is already called is not a rename.
            ("paper.pdf", "That is already its name."),
        ] {
            harness
                .screen
                .rename
                .as_mut()
                .unwrap()
                .field
                .set_text(typed);
            // Confirming a refused name does nothing and leaves the dialogue open to be fixed.
            assert_eq!(
                verb_of(harness.press(KeyInput::Enter, Modifiers::default())),
                None,
                "{typed:?} was accepted"
            );
            assert!(
                harness.screen.rename.is_some(),
                "{typed:?} closed the dialogue"
            );
            assert_eq!(
                harness.screen.rename.as_ref().unwrap().error,
                Some(reason),
                "{typed:?} gave the wrong reason"
            );
        }

        // Escape abandons it, and nothing was renamed.
        harness.press(KeyInput::Escape, Modifiers::default());
        assert!(harness.screen.rename.is_none());
    }

    #[test]
    fn file_actions_are_reachable_by_pointer_and_every_one_is_a_declared_action() {
        // The verb row: 220 px roots rail, then the toolbar, then this row at y 42..84.
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&["/home/test/Downloads/paper.pdf"]);
        let clicked = verb_of(harness.click(260.0, 62.0));
        assert_eq!(
            clicked.map(|(verb, _)| verb),
            Some(FileVerb::Copy),
            "the first button on the verb row is Copy"
        );

        // Every verb on the row is a declared action, and the row is what makes them reachable
        // without a keyboard at all.
        for route in FILE_VERB_ROUTES {
            assert_eq!(route.verb.action_id(), route.action);
            assert!(
                lege_types::action::action(route.action).is_some(),
                "{} is on the verb row and in no manifest",
                route.action
            );
        }
        assert_eq!(FILE_VERB_ROUTES.len(), 8);
    }

    #[test]
    fn file_actions_that_need_one_item_refuse_a_multiple_selection() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&[
            "/home/test/Downloads/paper.pdf",
            "/home/test/Downloads/Folder",
        ]);
        // Copy, cut and trash are bulk verbs and take the whole selection as one request.
        for (key, modifiers, verb) in [
            (KeyInput::Character('c'), control(), FileVerb::Copy),
            (KeyInput::Delete, Modifiers::default(), FileVerb::Trash),
        ] {
            let (got, paths) = verb_of(harness.press(key, modifiers)).expect("a bulk verb");
            assert_eq!(got, verb);
            assert_eq!(
                paths.len(),
                2,
                "one request naming both, never two requests"
            );
        }
        // Rename, Properties and Copy path have no single answer for a dozen items, so the
        // surface does not ask rather than asking about an arbitrary one of them.
        for (key, modifiers) in [
            (KeyInput::Function(2), Modifiers::default()),
            (KeyInput::Character('c'), control_shift()),
        ] {
            assert_eq!(verb_of(harness.press(key, modifiers)), None);
        }
    }

    #[test]
    fn file_actions_do_nothing_with_nothing_selected() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&[]);
        for (key, modifiers) in [
            (KeyInput::Character('c'), control()),
            (KeyInput::Character('x'), control()),
            (KeyInput::Delete, Modifiers::default()),
            (KeyInput::Function(2), Modifiers::default()),
        ] {
            assert_eq!(
                verb_of(harness.press(key, modifiers)),
                None,
                "a verb that acts on a selection must not fire without one"
            );
        }
        // Paste and New folder act on the folder being shown and are still offered.
        assert!(matches!(
            harness.press(KeyInput::Character('v'), control()),
            Some(FileBrowserAction::File(FileRequest {
                verb: FileVerb::Paste,
                ..
            }))
        ));
    }

    #[test]
    fn file_browser_pointer_activates_folders_and_files() {
        let mut harness = Harness::new();
        harness.frame();
        // Browser begins after the 220 px roots rail; entries begin below the toolbar, the
        // file-verb row and the column header — 42 + 42 + 28 px.
        assert_eq!(
            harness.click(400.0, 130.0),
            Some(FileBrowserAction::Browse {
                drawer: Drawer::Downloads,
                path: PathBuf::from("/home/test/Downloads/Folder"),
                show_hidden: false,
            })
        );
        assert_eq!(
            harness.click(400.0, 168.0),
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
        harness.input.cursor_moved(400.0, 130.0);
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
        harness.input.cursor_moved(400.0, 168.0);
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
