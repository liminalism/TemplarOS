//! The Files object's browse view: real service entries, the pinned locations, and direct
//! navigation.
//!
//! It owns no state about *where* it is or what is selected — all of that lives in
//! [`FilesSurfaceState`], which outlives every collapse of the object, so this is a view over
//! that state and nothing more. What it still owns is the one piece of genuinely transient UI:
//! the rename dialogue, which exists only while a question is on screen.

use std::path::{Path, PathBuf};

use lege_types::file_sort::{SortDescriptor, SortField, ViewMode};
use pixelkit_raster::Bitmap;
use pixelkit_raster::{Rect, ScaleFilter};
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::Ui;
use std::collections::HashMap;

use crate::file_browser::{BrowseEntry, BrowserSources, DirectoryListing};
use crate::files_surface::{FilesLocation, FilesSurfaceState, FocusArea};

const ROOTS_WIDTH: i32 = 220;
const ROOT_ROW_HEIGHT: i32 = 46;

/// The side of a pinned row's icon, logical pixels.
const PINNED_ICON: i32 = 20;

/// The key a theme icon is filed under in the shared icon map. Prefixed so a freedesktop icon
/// name can never collide with a canonical application id.
pub fn theme_icon_key(name: &str) -> String {
    format!("theme:{name}")
}
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
    /// Hand the question to the `file-search` provider — the application that searches every
    /// file on the machine, rather than the folder being looked at.
    SearchHere,
    /// Start the selected `terminal` provider in this folder — the row's own directory, or the
    /// folder holding the file. Where exactly is `Files.Actions1.TerminalDirectory`'s answer,
    /// not this surface's guess.
    OpenTerminal,
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
        !matches!(self, Self::Paste | Self::NewFolder | Self::SearchHere)
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
            Self::OpenTerminal => "Open terminal here",
            Self::SearchHere => "Search here…",
        }
    }

    /// The verb one declared action performs, for a menu row that names the action rather than
    /// the verb. `None` for an action this surface answers itself — selecting everything and
    /// changing the sort are moves of the view, not requests to a service.
    pub fn for_action(action: &str) -> Option<FileVerb> {
        FILE_VERB_ROUTES
            .iter()
            .find(|route| route.action == action)
            .map(|route| route.verb)
            .or(match action {
                "files.open-terminal" => Some(Self::OpenTerminal),
                "files.search-here" => Some(Self::SearchHere),
                _ => None,
            })
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
    /// The question a search verb hands on, when this surface is showing one. `None` for every
    /// other verb, and for a folder being browsed rather than searched.
    pub query: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileBrowserAction {
    /// Fetch what the surface's current location needs. Navigation itself is not an action —
    /// the view moves [`FilesSurfaceState`] and the host simply keeps up with where it is.
    Load {
        location: FilesLocation,
        show_hidden: bool,
    },
    Open(PathBuf),
    /// Hand this folder to the standalone Files manager — the surface with the full drives
    /// sidebar and properties. Home's pane is the quick look; this is the door to the deep one.
    OpenInFiles(PathBuf),
    /// An ordinary file verb, for the host to perform through `lege-filed`.
    File(FileRequest),
}

/// One consistent snapshot of the app-owned async state. Paths are paired with the drawer and
/// hidden-files mode that requested them so a slow response can never replace a newer view.
#[derive(Debug)]
pub struct FileBrowserData<'a> {
    pub sources: &'a BrowserSources,
    pub listing: Option<(&'a FilesLocation, bool, &'a DirectoryListing)>,
    pub pending: Option<(&'a FilesLocation, bool)>,
    pub error: Option<(&'a FilesLocation, &'a str)>,
    pub status: Option<&'a str>,
    pub search_query: Option<&'a str>,
    pub search_entries: Option<&'a [BrowseEntry]>,
    pub search_error: Option<&'a str>,
    /// The rows a feed location shows, already rendered by `crate::drawers`.
    pub feeds: &'a HashMap<crate::home_surface::Drawer, crate::drawers::DrawerFeed>,
    /// Decoded icons by lookup key, shared with the field's own map: an application's is its
    /// canonical id, a theme icon's is `theme:<freedesktop name>`.
    pub icons: &'a HashMap<String, Bitmap>,
    /// The user's home directory, so the one folder whose icon its name cannot give away gets
    /// the right one.
    pub home: Option<&'a Path>,
    /// Decoded thumbnails by path, produced off the render thread by the host from the bounded
    /// requests [`FileBrowserScreen::begin_thumbnails`] hands it. Drawing only ever reads.
    pub thumbnails: &'a HashMap<PathBuf, Bitmap>,
}

/// The browse view. Everything durable it draws lives in [`FilesSurfaceState`]; this holds only
/// the dialogue that is on screen right now.
#[derive(Debug, Default)]
pub struct FilesBrowseView {
    /// The open rename dialogue, if any.
    rename: Option<RenameDialog>,
    /// The secondary-click menu over a row or over the ground between rows. The field's widget
    /// (`crate::surface::context_menu`) with the Files object's own row table, so a verb cannot
    /// mean one thing on the toolbar and another in the menu.
    menu: crate::surface::context_menu::ContextMenu,
    menu_on_ground: bool,
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

impl FilesBrowseView {
    #[allow(clippy::too_many_lines)]
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
        data: &FileBrowserData<'_>,
        keyboard_enabled: bool,
    ) -> Option<FileBrowserAction> {
        let here = state.location().clone();
        let listing = data
            .listing
            .filter(|(owner, hidden, _)| **owner == here && *hidden == state.show_hidden)
            .map(|(_, _, listing)| listing);
        let pending = data
            .pending
            .is_some_and(|(owner, hidden)| *owner == here && hidden == state.show_hidden);
        let searching = matches!(here, FilesLocation::Search { .. });
        // A feed location's rows are already rendered text, not entries: nothing to select and
        // nothing to sort, so it takes the simple path and returns early.
        if let Some(drawer) = here.feed() {
            Self::draw_feed(ui, area, state, drawer, data);
            return None;
        }
        let ordered: Vec<BrowseEntry>;
        let visible_entries: &[BrowseEntry] = if searching {
            data.search_entries.unwrap_or(&[])
        } else if state.sort == SortDescriptor::default() {
            listing.map_or(&[], |listing| listing.entries.as_slice())
        } else {
            ordered = FilesSurfaceState::sorted(
                listing.map_or(&[], |listing| listing.entries.as_slice()),
                state.sort,
            );
            &ordered
        };
        state.retain_selection(visible_entries);
        if !pending {
            state.observe(listing.map(|listing| listing.directory.as_path()));
        }

        // An open menu owns this frame's input, and it is consulted before anything underneath
        // is drawn — the same two-step the field's menu follows, and for the same reason: the
        // menu is painted over the listing, so the row beneath would otherwise take the click.
        let menu_directory = listing.map(|listing| listing.directory.clone());
        let mut action = if self.menu.is_open() {
            self.menu.update(ui, area).and_then(|chosen| {
                self.run_menu_action(
                    chosen.action,
                    state,
                    menu_directory.as_deref(),
                    visible_entries,
                )
            })
        } else {
            None
        };

        // The rename dialogue owns the frame while it is open: it takes the keys, so nothing
        // underneath can act on a keystroke meant for the name being typed.
        let renaming = self.rename.is_some();
        if action.is_none() && keyboard_enabled && !renaming && !self.menu.is_open() {
            let width = area.w - ui.px(ROOTS_WIDTH).min(area.w / 3) - ui.px(16);
            let columns = if state.view == ViewMode::Thumbnails && !searching {
                (width / ui.px(120).max(40)).max(1) as usize
            } else {
                1
            };
            action = self.handle_keys(ui, state, data.sources, listing, visible_entries, columns);
        }

        ui.panel(area);
        let (roots_area, browser_area) = area.split_left(ui.px(ROOTS_WIDTH).min(area.w / 3));
        Self::draw_pinned(ui, roots_area, state, data.sources, data.icons, data.home);

        let browser_area = browser_area.inset(ui.px(8));
        let (toolbar, below_toolbar) = browser_area.split_top(ui.px(TOOLBAR_HEIGHT * 2));
        let toolbar_action = Self::draw_toolbar(ui, toolbar, state, listing, searching);
        if action.is_none() {
            action = toolbar_action;
        }
        let (verbs, below_verbs) = below_toolbar.split_top(ui.px(TOOLBAR_HEIGHT));
        let verb_action = self.draw_file_verbs(ui, verbs, state, listing);
        if action.is_none() {
            action = verb_action;
        }
        let entries_area = if state.view == ViewMode::List || searching {
            let (columns, entries_area) = below_verbs.split_top(ui.px(COLUMNS_HEIGHT));
            Self::draw_columns(ui, columns);
            entries_area
        } else {
            below_verbs
        };

        let error = data.search_error.or_else(|| {
            data.error
                .filter(|(owner, _)| **owner == here)
                .map(|(_, message)| message)
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
            } else {
                let drawn = self.draw_entries(ui, entries_area, state, visible_entries, false);
                if action.is_none() {
                    action = drawn;
                }
            }
        } else if pending {
            ui.label(
                entries_area.inset(ui.px(12)),
                &format!("Loading {}…", here.label()),
                ui.theme.text_dim,
                Align::Left,
            );
        } else if let Some(listing) = listing {
            let drawn = if state.view == ViewMode::Thumbnails {
                self.draw_thumbnails(ui, entries_area, state, visible_entries, data.thumbnails)
            } else {
                self.draw_entries(ui, entries_area, state, visible_entries, listing.truncated)
            };
            if action.is_none() {
                action = drawn;
            }
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

        // Whatever the rows did not take is the ground between them, and the ground has a menu
        // of its own: New folder, Paste, Select all, Sort by…, Show hidden — the five things
        // that are about the folder rather than about one file in it.
        if !self.menu.is_open() && ui.input.take_context_click(entries_area) {
            let target = crate::surface::context_menu::FilesTarget {
                row: false,
                directory: state.directory().is_some(),
            };
            if let Some(at) = ui.input.cursor() {
                self.menu_on_ground = true;
                self.menu.open_rows_at(at, target.rows());
            }
        }
        // Painted over everything the object drew this frame; its input was taken at the top.
        self.menu.paint(ui, area);

        // Drawn last so it sits over the listing, and consulted before the automatic load
        // below: a dialogue waiting for an answer must not be reloaded out from under itself.
        let directory = listing.map(|listing| listing.directory.as_path());
        if let Some(renamed) = self.draw_rename(ui, entries_area, state, directory) {
            return Some(renamed);
        }
        if self.rename.is_some() {
            return action;
        }

        // Wherever the surface says it is, that is what the host fetches. Navigation is a move
        // of `state`, never a request in itself, so Back and a fresh click take the same path.
        let settled = state.location() == &here;
        if action.is_none() && (!settled || (listing.is_none() && !pending && error.is_none())) {
            action = Some(FileBrowserAction::Load {
                location: state.location().clone(),
                show_hidden: state.show_hidden,
            });
        }
        action
    }

    /// A feed location — Recent, Drives, Trash, Operations — as the rows `crate::drawers`
    /// already rendered. One place, one listing: these are not a different kind of surface, only
    /// a location whose rows come from a service that counts rather than from a directory.
    fn draw_feed(
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
        drawer: crate::home_surface::Drawer,
        data: &FileBrowserData<'_>,
    ) {
        ui.panel(area);
        let (roots_area, body) = area.split_left(ui.px(ROOTS_WIDTH).min(area.w / 3));
        Self::draw_pinned(ui, roots_area, state, data.sources, data.icons, data.home);
        let body = body.inset(ui.px(8));
        let feed = data.feeds.get(&drawer);
        let (heading, rows) = body.split_top(ui.px(26));
        ui.label(
            heading,
            feed.map_or("…", |feed| feed.headline.as_str()),
            ui.theme.text,
            Align::Left,
        );
        let lines = feed.map_or(&[][..], |feed| feed.lines.as_slice());
        if lines.is_empty() {
            ui.label(
                Rect::new(rows.x, rows.y, rows.w, ui.px(ENTRY_HEIGHT)),
                "Nothing here.",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let heights = vec![ui.px(ENTRY_HEIGHT); lines.len()];
        ui.scroll_list(&mut state.entry_scroll, rows, &heights, |ui, index, row| {
            if ui.input.hovering(row) {
                ui.painter.fill_rect(row, ui.theme.hover);
            } else if index % 2 == 1 {
                ui.painter.fill_rect(row, ui.theme.panel_alt);
            }
            ui.label(
                row.inset(ui.px(7)),
                &lines[index],
                ui.theme.text,
                Align::Left,
            );
        });
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
    fn request(
        &mut self,
        verb: FileVerb,
        state: &FilesSurfaceState,
        directory: Option<&Path>,
    ) -> Option<FileBrowserAction> {
        if verb == FileVerb::Rename {
            let selected = state.selection.to_vec();
            let [only] = selected.as_slice() else {
                return None;
            };
            // Opening the dialogue *is* performing Rename; the request comes with the name.
            self.rename = Some(RenameDialog::new(only.clone()));
            return None;
        }
        Self::can_request(verb, state, directory)
    }

    /// Perform one row of the context menu.
    ///
    /// Every row that names a file verb goes through [`Self::request`] — the same call the
    /// toolbar button and the accelerator make — so the three routes cannot disagree about what
    /// a verb does or about when it applies. The rest are moves of the view itself: selecting
    /// everything, changing the sort and showing hidden files are what the toolbar's own
    /// controls do, and they reach the same state.
    fn run_menu_action(
        &mut self,
        action: &'static str,
        state: &mut FilesSurfaceState,
        directory: Option<&Path>,
        entries: &[BrowseEntry],
    ) -> Option<FileBrowserAction> {
        match action {
            "files.open-terminal" if self.menu_on_ground => {
                Some(FileBrowserAction::File(FileRequest {
                    verb: FileVerb::OpenTerminal,
                    paths: vec![directory?.to_path_buf()],
                    directory: directory.map(Path::to_path_buf),
                    new_name: None,
                    query: None,
                }))
            }
            "files.open" => {
                let entry = entries.get(state.focus)?;
                let entry = entry.clone();
                activate(state, &entry)
            }
            "files.select-all" => {
                let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
                state.selection.select_all(&order);
                state.focus_area = FocusArea::Entries;
                None
            }
            "files.sort" => {
                state.cycle_sort_field(entries);
                None
            }
            "files.show-hidden" => {
                state.show_hidden = !state.show_hidden;
                Some(FileBrowserAction::Load {
                    location: state.location().clone(),
                    show_hidden: state.show_hidden,
                })
            }
            other => {
                let verb = FileVerb::for_action(other)?;
                self.request(verb, state, directory)
            }
        }
    }

    /// Whether `verb` applies right now, and the request it would make. No side effects.
    fn can_request(
        verb: FileVerb,
        state: &FilesSurfaceState,
        directory: Option<&Path>,
    ) -> Option<FileBrowserAction> {
        let paths = state.selection.to_vec();
        if verb.needs_selection() && paths.is_empty() {
            return None;
        }
        // Rename and Properties are about one item; asking for either with a dozen selected has
        // no single answer, so the surface does not ask.
        if matches!(
            verb,
            FileVerb::Rename | FileVerb::Properties | FileVerb::CopyPath | FileVerb::OpenTerminal
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
            // Only a search verb carries one, and only when this surface is showing a search:
            // the question a person has already typed is the question they mean to widen.
            query: match (verb, state.location()) {
                (FileVerb::SearchHere, FilesLocation::Search { query, .. }) => Some(query.clone()),
                _ => None,
            },
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn handle_keys(
        &mut self,
        ui: &Ui<'_>,
        state: &mut FilesSurfaceState,
        sources: &BrowserSources,
        listing: Option<&DirectoryListing>,
        entries: &[BrowseEntry],
        columns: usize,
    ) -> Option<FileBrowserAction> {
        let mut action = None;
        let modifiers = ui.input.modifiers();
        let directory = listing.map(|listing| listing.directory.as_path());
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        for key in ui.input.keys() {
            match key {
                KeyInput::Left if modifiers.alt => {
                    state.go_back();
                }
                KeyInput::Right if modifiers.alt => {
                    state.go_forward();
                }
                KeyInput::Up if modifiers.alt => {
                    state.go_up(listing.and_then(|listing| listing.parent.as_deref()));
                }
                KeyInput::Function(5) => {
                    action = Some(FileBrowserAction::Load {
                        location: state.location().clone(),
                        show_hidden: state.show_hidden,
                    });
                }
                KeyInput::Character('h' | 'H') if modifiers.control => {
                    state.show_hidden = !state.show_hidden;
                    action = Some(FileBrowserAction::Load {
                        location: state.location().clone(),
                        show_hidden: state.show_hidden,
                    });
                }
                KeyInput::Left | KeyInput::Right
                    if state.view == ViewMode::Thumbnails
                        && state.focus_area == FocusArea::Entries =>
                {
                    let to = if *key == KeyInput::Left {
                        state.focus.saturating_sub(1)
                    } else {
                        state.focus.saturating_add(1)
                    };
                    state.focus_entry(to, entries, modifiers.shift);
                }

                // The ordinary file verbs. Surface-local accelerators, exactly like Ctrl+G and
                // Ctrl+S beneath them: these are not compositor-reserved chords, so they cost
                // the recovery keys nothing and work only where a file surface has focus.
                KeyInput::Character('c' | 'C') if modifiers.control && modifiers.shift => {
                    action = action.or_else(|| self.request(FileVerb::CopyPath, state, directory));
                }
                KeyInput::Character('c' | 'C') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Copy, state, directory));
                }
                KeyInput::Character('x' | 'X') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Cut, state, directory));
                }
                KeyInput::Character('v' | 'V') if modifiers.control => {
                    action = action.or_else(|| self.request(FileVerb::Paste, state, directory));
                }
                KeyInput::Character('n' | 'N') if modifiers.control && modifiers.shift => {
                    action = action.or_else(|| self.request(FileVerb::NewFolder, state, directory));
                }
                KeyInput::Delete => {
                    action = action.or_else(|| self.request(FileVerb::Trash, state, directory));
                }
                KeyInput::Function(2) => {
                    action = action.or_else(|| self.request(FileVerb::Rename, state, directory));
                }
                KeyInput::Enter if modifiers.alt => {
                    action =
                        action.or_else(|| self.request(FileVerb::Properties, state, directory));
                }
                KeyInput::Character('g') if modifiers.control => state.toggle_view(),
                KeyInput::Character('s') if modifiers.control => state.cycle_sort_field(entries),
                KeyInput::Character('r') if modifiers.control => {
                    state.toggle_sort_direction(entries);
                }
                KeyInput::Left => state.focus_area = FocusArea::Pinned,
                KeyInput::Right => state.focus_area = FocusArea::Entries,
                KeyInput::Tab => {
                    state.focus_area = match state.focus_area {
                        FocusArea::Pinned => FocusArea::Entries,
                        FocusArea::Entries => FocusArea::Pinned,
                    };
                }
                KeyInput::Up => match state.focus_area {
                    FocusArea::Pinned => {
                        state.pinned_focus = state.pinned_focus.saturating_sub(1);
                    }
                    FocusArea::Entries => {
                        let to = state.focus.saturating_sub(columns);
                        state.focus_entry(to, entries, modifiers.shift);
                    }
                },
                KeyInput::Down => match state.focus_area {
                    FocusArea::Pinned => {
                        state.pinned_focus =
                            (state.pinned_focus + 1).min(sources.pinned.len().saturating_sub(1));
                    }
                    FocusArea::Entries => {
                        let to = state.focus.saturating_add(columns);
                        state.focus_entry(to, entries, modifiers.shift);
                    }
                },
                KeyInput::Home => match state.focus_area {
                    FocusArea::Pinned => state.pinned_focus = 0,
                    FocusArea::Entries => state.focus_entry(0, entries, modifiers.shift),
                },
                KeyInput::End => match state.focus_area {
                    FocusArea::Pinned => {
                        state.pinned_focus = sources.pinned.len().saturating_sub(1);
                    }
                    FocusArea::Entries => {
                        state.focus_entry(
                            entries.len().saturating_sub(1),
                            entries,
                            modifiers.shift,
                        );
                    }
                },
                KeyInput::Backspace => {
                    state.go_up(listing.and_then(|listing| listing.parent.as_deref()));
                }
                KeyInput::Character('a' | 'A') if modifiers.control => {
                    state.selection.select_all(&order);
                    state.focus = 0;
                    state.focus_area = FocusArea::Entries;
                }
                KeyInput::Enter if !modifiers.alt => match state.focus_area {
                    FocusArea::Pinned => {
                        if let Some(pinned) = sources.pinned.get(state.pinned_focus) {
                            state.go_to(pinned.location.clone());
                        }
                    }
                    FocusArea::Entries => {
                        if let Some(entry) = entries.get(state.focus) {
                            action = activate(state, entry);
                        }
                    }
                },
                _ => {}
            }
        }
        action
    }

    /// The pinned locations: the machine's own file feeds and every place and volume worth one
    /// press. Drawn only at the expanded rung, where there is room for it to be a list rather
    /// than a squeeze.
    fn draw_pinned(
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
        sources: &BrowserSources,
        icons: &HashMap<String, Bitmap>,
        home: Option<&Path>,
    ) {
        ui.painter.fill_rect(area, ui.theme.panel_alt);
        let (title, body) = area.inset(ui.px(6)).split_top(ui.px(30));
        ui.label_styled(
            title,
            "LOCATIONS",
            TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
            ui.theme.text_dim,
            Align::Left,
        );
        let heights = vec![ui.px(ROOT_ROW_HEIGHT); sources.pinned.len()];
        let mut chosen = None;
        let here = state.location().clone();
        let focus_area = state.focus_area;
        let pinned_focus = state.pinned_focus;
        ui.scroll_list(
            &mut state.pinned_scroll,
            body,
            &heights,
            |ui, index, row| {
                let pinned = &sources.pinned[index];
                let focused = focus_area == FocusArea::Pinned && pinned_focus == index;
                let active = pinned.location == here;
                if focused || active {
                    ui.painter.fill_rect(row, ui.theme.selection);
                } else if ui.input.hovering(row) {
                    ui.painter.fill_rect(row, ui.theme.hover);
                }
                // The icon says what kind of thing this is and the name says which one. A second
                // line saying "Standard folder" under every folder was the same word repeated
                // down the rail (the fifth hands-on run), so the detail line is drawn only when
                // it carries something the name does not — a volume's free space, or that a
                // place named in the user's directories is not there.
                let body = row.inset(ui.px(5));
                let (icon_box, text) = body.split_left(ui.px(PINNED_ICON) + ui.px(6));
                if let Some(bitmap) = icons.get(&theme_icon_key(pinned.icon_name(home))) {
                    let side = ui.px(PINNED_ICON);
                    let square =
                        Rect::new(icon_box.x, icon_box.y + (icon_box.h - side) / 2, side, side);
                    ui.painter.blit(bitmap, square, ScaleFilter::Bilinear);
                }
                if pinned.detail.is_empty() {
                    ui.label(text, &pinned.label, ui.theme.text, Align::Left);
                } else {
                    let (name_row, detail_row) = text.split_top(ui.px(21));
                    ui.label(name_row, &pinned.label, ui.theme.text, Align::Left);
                    ui.label_styled(
                        detail_row,
                        &pinned.detail,
                        TextStyle::new(ui.theme.body.face, 11.0),
                        ui.theme.text_dim,
                        Align::Left,
                    );
                }
                if ui.input.take_click(row) {
                    chosen = Some(index);
                }
            },
        );
        if let Some(index) = chosen {
            state.focus_area = FocusArea::Pinned;
            state.pinned_focus = index;
            state.go_to(sources.pinned[index].location.clone());
        }
    }

    fn navigation_button(ui: &mut Ui<'_>, area: Rect, label: &str, enabled: bool) -> bool {
        let area = area.inset(ui.px(4));
        if enabled {
            ui.button(area, label)
        } else {
            ui.label(area, label, ui.theme.text_dim, Align::Centre);
            false
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_toolbar(
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
        listing: Option<&DirectoryListing>,
        searching: bool,
    ) -> Option<FileBrowserAction> {
        let current = state.directory().map(Path::to_path_buf);
        let current = current.as_deref();
        // Navigation keeps its own row so the current path survives in a narrow window.
        let (navigation, options) = area.split_top(ui.px(TOOLBAR_HEIGHT));
        let (back, rest) = navigation.split_left(ui.px(64));
        let (forward, rest) = rest.split_left(ui.px(88));
        let (files, path_area) = rest.split_left(ui.px(72));
        let (hidden, rest) = options.split_right(ui.px(116));
        let (view, rest) = rest.split_right(ui.px(96));
        let (direction, rest) = rest.split_right(ui.px(72));
        let (sort, summary) = rest.split_right(ui.px(110));
        let (refresh, summary) = summary.split_left(ui.px(88));
        if ui.button(refresh.inset(ui.px(4)), "Refresh") {
            return Some(FileBrowserAction::Load {
                location: state.location().clone(),
                show_hidden: state.show_hidden,
            });
        }
        let entries: &[BrowseEntry] = listing.map_or(&[], |listing| listing.entries.as_slice());
        let count = if state.selection.is_empty() {
            format!("{} items", entries.len())
        } else {
            format!("{} of {} selected", state.selection.len(), entries.len())
        };
        if !searching {
            ui.label(
                summary.inset(ui.px(6)),
                &count,
                ui.theme.text_dim,
                Align::Left,
            );
        }
        let sort_label = format!(
            "Sort: {}",
            match state.sort.field {
                SortField::NaturalName => "name",
                SortField::Kind => "type",
                SortField::Size => "size",
                SortField::Modified => "date",
                SortField::Manual => "manual",
            }
        );
        if ui.button(sort.inset(ui.px(4)), &sort_label) {
            state.cycle_sort_field(entries);
        }
        if ui.button(
            direction.inset(ui.px(4)),
            if state.sort.ascending { "Asc" } else { "Desc" },
        ) {
            state.toggle_sort_direction(entries);
        }
        if ui.button(
            view.inset(ui.px(4)),
            match state.view {
                ViewMode::List => "View: list",
                ViewMode::Thumbnails => "View: grid",
            },
        ) {
            state.toggle_view();
        }
        if Self::navigation_button(ui, back, "Back", state.can_back()) {
            state.go_back();
        }
        if Self::navigation_button(ui, forward, "Forward", state.can_forward()) {
            state.go_forward();
        }
        if ui.button(files.inset(ui.px(4)), "Files")
            && let Some(path) = current
        {
            return Some(FileBrowserAction::OpenInFiles(path.to_path_buf()));
        }
        if ui.button(
            hidden.inset(ui.px(4)),
            if state.show_hidden {
                "Hidden: on"
            } else {
                "Hidden: off"
            },
        ) {
            state.show_hidden = !state.show_hidden;
            if let Some(path) = current {
                return Some(FileBrowserAction::Load {
                    location: FilesLocation::Directory(path.to_path_buf()),
                    show_hidden: state.show_hidden,
                });
            }
        }
        if searching {
            ui.label(
                path_area.inset(ui.px(8)),
                &state.location().label(),
                ui.theme.text,
                Align::Left,
            );
            return None;
        }
        let Some(path) = current else {
            ui.label(
                path_area.inset(ui.px(8)),
                "No location",
                ui.theme.text_dim,
                Align::Left,
            );
            return None;
        };
        let (up, path_area) = path_area.split_left(ui.px(48).min(path_area.w));
        if Self::navigation_button(ui, up, "Up", path.parent().is_some()) {
            state.go_up(path.parent());
        }
        Self::draw_breadcrumb(ui, path_area, state, path);
        None
    }

    /// The location as a row of its ancestors, each one a step the pointer can take — the
    /// answer to "how do I get to a specific folder" being the path itself. When the row is
    /// deeper than the space, the leading ancestors give way: the folders nearest to where the
    /// person is are the ones worth a button.
    fn draw_breadcrumb(ui: &mut Ui<'_>, area: Rect, state: &mut FilesSurfaceState, path: &Path) {
        let mut prefixes: Vec<(String, PathBuf)> = Vec::new();
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            let label = match component {
                std::path::Component::RootDir => "/".to_owned(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            prefixes.push((label, prefix.clone()));
        }
        // Fit from the tail: measure right-to-left, then draw whatever fits left-to-right. The
        // widths are the rendered text's own, not a per-character guess — a guess that runs
        // short elides "home" to "ho…" in a row with room to spare.
        let pad = ui.px(10) + ui.px(8);
        let style = ui.theme.body;
        let widths: Vec<i32> = prefixes
            .iter()
            .map(|(label, _)| pad + ui.text.measure(label, style))
            .collect();
        let mut width_needed = 0;
        let mut first_shown = prefixes.len();
        for width in widths.iter().rev() {
            if width_needed + width > area.w {
                break;
            }
            width_needed += width;
            first_shown -= 1;
        }
        // Always show at least the folder itself, however long its name.
        first_shown = first_shown.min(prefixes.len().saturating_sub(1));
        let mut x = area.x;
        let mut chosen = None;
        for (index, (label, target)) in prefixes.iter().enumerate().skip(first_shown) {
            let width = widths[index].min(area.right() - x);
            if width <= pad / 2 {
                break;
            }
            let slot = Rect::new(x, area.y, width, area.h);
            let last = index + 1 == prefixes.len();
            if last {
                ui.label(slot.inset(ui.px(4)), label, ui.theme.text, Align::Centre);
            } else if ui.button(slot.inset(ui.px(4)), label) {
                chosen = Some(target.clone());
            }
            x += width;
        }
        if let Some(target) = chosen {
            state.go_to(FilesLocation::Directory(target));
        }
    }

    /// The rename dialogue: a name field, a refusal, and two buttons.
    ///
    /// Returns the confirmed request, and consumes the frame's keys while open so a stray Delete
    /// aimed at the field cannot trash the item being renamed.
    fn draw_rename(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        _state: &FilesSurfaceState,
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
            query: None,
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
        state: &mut FilesSurfaceState,
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
            let enabled = Self::can_request(verb, state, directory).is_some();
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
        self.request(pressed?, state, directory)
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

    fn reveal_keyboard_focus(
        ui: &Ui<'_>,
        state: &mut FilesSurfaceState,
        area: Rect,
        row_height: i32,
        columns: usize,
        count: usize,
    ) {
        if state.focus_area == FocusArea::Entries
            && ui.input.keys().iter().any(|key| {
                matches!(
                    key,
                    KeyInput::Up
                        | KeyInput::Down
                        | KeyInput::Left
                        | KeyInput::Right
                        | KeyInput::Home
                        | KeyInput::End
                )
            })
        {
            state.entry_scroll.reveal(
                (state.focus / columns) as i32 * row_height,
                row_height,
                area.h,
                count.div_ceil(columns) as i32 * row_height,
            );
        }
    }

    /// The thumbnail grid: the same entries, selection and focus as the list, drawn as
    /// responsive preview cells. Cells without a decoded bitmap show the entry's kind chip; the
    /// grid itself never reads a file.
    #[allow(clippy::too_many_lines)]
    fn draw_thumbnails(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
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
        Self::reveal_keyboard_focus(ui, state, area, cell, columns, entries.len());
        let heights = vec![cell; rows];
        let mut chosen = None;
        let mut secondary = None;
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        let mut seen: Option<(usize, usize)> = None;
        ui.scroll_list(
            &mut state.entry_scroll,
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
                    let focused = state.focus_area == FocusArea::Entries && state.focus == index;
                    let in_selection = state.selection.contains(&entry.path);
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
                    } else if entry.is_dir() {
                        // Folder-ness is a material, not a caption: the warm folder shell the
                        // Home Field already uses for folder objects, with no text to repeat it.
                        let plate = image.inset(ui.px(6));
                        ui.painter.fill_rounded_rect(
                            plate,
                            ui.px(crate::surface::design::CHIP_RADIUS),
                            crate::surface::design::folder_shell(),
                        );
                        ui.painter.stroke_rect(
                            plate,
                            ui.scale.hairline(),
                            crate::surface::design::folder_edge(),
                        );
                    } else {
                        // A file with an extension names it once; one without shows nothing —
                        // an all-caps FILE label repeats what the surface already says.
                        let chip = entry
                            .path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .map_or_else(String::new, str::to_uppercase);
                        ui.label(image, &chip, ui.theme.text_dim, Align::Centre);
                    }
                    ui.label(
                        caption.inset(ui.px(2)),
                        &entry.name,
                        ui.theme.text,
                        Align::Centre,
                    );
                    if ui.input.take_context_click(tile) {
                        state.focus_area = FocusArea::Entries;
                        state.focus = index;
                        if !state.selection.contains(&entry.path) {
                            state.selection.replace(entry.path.clone());
                        }
                        secondary = ui.input.cursor();
                    }
                    if ui.input.take_click(tile) {
                        state.focus_area = FocusArea::Entries;
                        let previous = state.focus;
                        state.focus = index;
                        let modifiers = ui.input.modifiers();
                        if modifiers.control {
                            state.selection.toggle(entry.path.clone());
                        } else if modifiers.shift {
                            if state.selection.is_empty()
                                && let Some(anchor) = entries.get(previous)
                            {
                                state.selection.replace(anchor.path.clone());
                            }
                            state.selection.extend_to(&order, entry.path.clone());
                        } else if state.selection.len() == 1
                            && state.selection.contains(&entry.path)
                        {
                            chosen = Some(entry.clone());
                        } else {
                            state.selection.replace(entry.path.clone());
                        }
                    }
                }
            },
        );
        if let Some(at) = secondary {
            self.menu_on_ground = false;
            self.menu.open_rows_at(
                at,
                crate::surface::context_menu::FilesTarget {
                    row: true,
                    directory: state.directory().is_some(),
                }
                .rows(),
            );
        }
        if let Some((low, high)) = seen {
            // One row of lookahead in each direction, so a slow scroll never shows a beat of
            // empty cells.
            state.saw_thumbnails(low.saturating_sub(columns)..(high + columns).min(entries.len()));
        }
        chosen.and_then(|entry| activate(state, &entry))
    }

    #[allow(clippy::too_many_lines)]
    fn draw_entries(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        state: &mut FilesSurfaceState,
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
        Self::reveal_keyboard_focus(ui, state, area, ui.px(ENTRY_HEIGHT), 1, entries.len());
        let heights = vec![ui.px(ENTRY_HEIGHT); entries.len()];
        let mut selected = None;
        let mut secondary: Option<(i32, i32)> = None;
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        ui.scroll_list(&mut state.entry_scroll, area, &heights, |ui, index, row| {
            let entry = &entries[index];
            let focused = state.focus_area == FocusArea::Entries && state.focus == index;
            let in_selection = state.selection.contains(&entry.path);
            if focused || in_selection {
                ui.painter.fill_rect(row, ui.theme.selection);
            } else if ui.input.hovering(row) {
                ui.painter.fill_rect(row, ui.theme.hover);
            } else if index % 2 == 1 {
                ui.painter.fill_rect(row, ui.theme.panel_alt);
            }
            let (modified, rest) = row.split_right(ui.px(150).min(row.w / 3));
            let (size, name) = rest.split_right(ui.px(100).min(rest.w / 3));
            // The name carries the identity; the row does not repeat what a person can see. A
            // trailing "/" is the whole folder marker, "→" the whole symlink one — the size
            // column's "—" already separates folders from files at a glance.
            let marker = if entry.is_dir() { "/" } else { "" };
            let link = if entry.is_symlink { " →" } else { "" };
            ui.label(
                name.inset(ui.px(7)),
                &format!("{}{marker}{link}", entry.name),
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
            // A secondary press is a menu about *this* row, so it selects it first — a menu
            // whose Rename acted on whatever happened to be selected elsewhere would be a menu
            // that lies about what it is over.
            if ui.input.take_context_click(row) {
                state.focus_area = FocusArea::Entries;
                state.focus = index;
                if !state.selection.contains(&entry.path) {
                    state.selection.replace(entry.path.clone());
                }
                secondary = ui.input.cursor();
            }
            if ui.input.take_click(row) {
                state.focus_area = FocusArea::Entries;
                let previous = state.focus;
                state.focus = index;
                let modifiers = ui.input.modifiers();
                if modifiers.control {
                    state.selection.toggle(entry.path.clone());
                } else if modifiers.shift {
                    if state.selection.is_empty()
                        && let Some(anchor) = entries.get(previous)
                    {
                        state.selection.replace(anchor.path.clone());
                    }
                    state.selection.extend_to(&order, entry.path.clone());
                } else if state.selection.len() == 1 && state.selection.contains(&entry.path) {
                    // The second press on the one selected row opens it. The first press only
                    // selects, so the file verbs — which act on a selection — have something to
                    // act on; a row that opened on its first press could never be copied,
                    // renamed or trashed by pointer at all.
                    selected = Some(entry.clone());
                } else {
                    state.selection.replace(entry.path.clone());
                }
            }
        });
        if let Some(at) = secondary {
            let target = crate::surface::context_menu::FilesTarget {
                row: true,
                directory: state.directory().is_some(),
            };
            self.menu_on_ground = false;
            self.menu.open_rows_at(at, target.rows());
        }
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
        selected.and_then(|entry| activate(state, &entry))
    }
}

/// Activating one row: a folder is somewhere to go, and going there is a move of the surface's
/// own state rather than an action for the host. A file is a thing to open, which the host does.
fn activate(state: &mut FilesSurfaceState, entry: &BrowseEntry) -> Option<FileBrowserAction> {
    if entry.is_dir() {
        state.go_to(FilesLocation::Directory(entry.path.clone()));
        None
    } else {
        Some(FileBrowserAction::Open(entry.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_browser::EntryKind;
    use crate::files_surface::{PinnedKind, PinnedLocation};
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, Modifiers, MouseButton, Scale};

    struct Harness {
        screen: FilesBrowseView,
        state: FilesSurfaceState,
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
            let mut state = FilesSurfaceState::default();
            state.go_to(FilesLocation::Directory(root.clone()));
            Self {
                screen: FilesBrowseView::default(),
                state,
                sources: BrowserSources {
                    pinned: vec![PinnedLocation {
                        label: "Downloads".into(),
                        detail: "Standard folder".into(),
                        location: FilesLocation::Directory(root.clone()),
                        kind: PinnedKind::Place,
                    }],
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
            let empty_feeds = std::collections::HashMap::new();
            let here = FilesLocation::Directory(self.listing.directory.clone());
            let searching = search_query.map(|query| FilesLocation::Search {
                root: self.listing.directory.clone(),
                query: query.to_owned(),
            });
            if let Some(searching) = searching {
                self.state.replace(searching);
            } else {
                self.state.replace(here.clone());
            }
            let data = FileBrowserData {
                sources: &self.sources,
                listing: Some((&here, self.state.show_hidden, &self.listing)),
                pending: None,
                error: None,
                status: None,
                search_query,
                search_entries,
                search_error: None,
                feeds: &empty_feeds,
                thumbnails: &empty_thumbnails,
                icons: &HashMap::new(),
                home: None,
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
                &mut self.state,
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

        /// Where the open menu actually put one of its rows. Asked of the menu rather than
        /// computed from its row height, so adding a row to `FILES_MENU` — which makes the popup
        /// taller and can push it up the screen to stay on it — does not move the click.
        fn menu_row(&mut self, index: usize) -> Rect {
            let ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                crate::surface::design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            self.screen
                .menu
                .row(&ui, Rect::new(0, 0, 1000, 600), index)
                .expect("the menu has that row")
        }
    }

    impl Harness {
        /// Secondary-click somewhere and report what the surface asked for.
        fn context_click(&mut self, x: f32, y: f32) -> Option<FileBrowserAction> {
            self.input.cursor_moved(x, y);
            self.input.mouse(MouseButton::Right, true);
            self.input.mouse(MouseButton::Right, false);
            self.frame()
        }

        /// Press a key with modifiers and report what the surface asked for.
        fn press(&mut self, key: KeyInput, modifiers: Modifiers) -> Option<FileBrowserAction> {
            self.input.modifiers_changed(modifiers);
            self.input.key(key);
            let action = self.frame();
            self.input.modifiers_changed(Modifiers::default());
            action
        }

        fn select(&mut self, paths: &[&str]) {
            self.state.selection.clear();
            for path in paths {
                self.state.selection.toggle(PathBuf::from(path));
            }
        }

        /// Where the surface says it is now — navigation is a move of the state, so this is what
        /// a gesture that used to return a `Browse` action is asserted against.
        fn here(&self) -> FilesLocation {
            self.state.location().clone()
        }
    }

    /// The ids a target's menu offers, in the order it offers them.
    fn menu_rows(row: bool, directory: bool) -> Vec<&'static str> {
        crate::surface::context_menu::FilesTarget { row, directory }
            .rows()
            .into_iter()
            .map(|item| item.action)
            .collect()
    }

    /// A row's menu is about one file; the ground's is about the folder. Neither offers the
    /// other's rows, because a menu that offers Paste over a file is a menu that has to explain
    /// which of the two things it meant.
    #[test]
    fn the_files_menu_offers_what_is_under_the_pointer() {
        assert_eq!(
            menu_rows(true, true),
            vec![
                "files.open",
                "files.open-terminal",
                "files.copy",
                "files.cut",
                "files.copy-path",
                "files.rename",
                "files.properties",
                "files.trash",
            ]
        );
        assert_eq!(
            menu_rows(false, true),
            vec![
                "files.open-terminal",
                "files.new-folder",
                "files.paste",
                "files.select-all",
                "files.sort",
                "files.show-hidden",
                "files.search-here",
            ]
        );
    }

    /// Recent and Trash are somewhere to look, not somewhere to put anything: no folder, so no
    /// Paste and no New folder. The rows that are about the view itself stay.
    #[test]
    fn a_feed_offers_no_paste_and_no_new_folder() {
        assert_eq!(
            menu_rows(false, false),
            vec![
                "files.select-all",
                "files.sort",
                "files.show-hidden",
                "files.search-here"
            ]
        );
    }

    /// The property the menu exists for: every row that names a file verb makes exactly the
    /// request the toolbar button of the same name makes. They are one call, not two code paths
    /// that happen to agree today.
    #[test]
    fn every_menu_row_asks_for_what_its_toolbar_button_asks_for() {
        let mut harness = Harness::new();
        harness.frame();
        harness.select(&["/home/test/Downloads/paper.pdf"]);
        let directory = PathBuf::from("/home/test/Downloads");
        let entries = harness.listing.entries.clone();

        for action in ["files.copy-path", "files.properties", "files.trash"] {
            let verb = FileVerb::for_action(action).expect("a declared file verb");
            let by_menu = harness.screen.run_menu_action(
                action,
                &mut harness.state,
                Some(&directory),
                &entries,
            );
            let by_toolbar = FilesBrowseView::can_request(verb, &harness.state, Some(&directory));
            assert_eq!(by_menu, by_toolbar, "{action}");
            assert!(by_menu.is_some(), "{action} applies to one selected file");
        }

        // Paste and New folder are about the folder, not the selection, and reach the same
        // request from the menu as from the row above the listing.
        for action in ["files.paste", "files.new-folder"] {
            let verb = FileVerb::for_action(action).expect("a declared file verb");
            assert_eq!(
                harness.screen.run_menu_action(
                    action,
                    &mut harness.state,
                    Some(&directory),
                    &entries
                ),
                FilesBrowseView::can_request(verb, &harness.state, Some(&directory)),
                "{action}"
            );
        }
    }

    #[test]
    fn terminal_on_directory_ground_ignores_selection_and_empty_folders_work() {
        let mut harness = Harness::new();
        harness.frame();
        let directory = PathBuf::from("/home/test/Downloads");
        let entries = harness.listing.entries.clone();
        for selected in [vec![], vec!["/home/test/Downloads/paper.pdf"]] {
            harness.select(&selected);
            harness.screen.menu_on_ground = true;
            let result = harness.screen.run_menu_action(
                "files.open-terminal",
                &mut harness.state,
                Some(&directory),
                &entries,
            );
            let Some(FileBrowserAction::File(request)) = result else {
                panic!("missing terminal request");
            };
            assert_eq!(request.verb, FileVerb::OpenTerminal);
            assert_eq!(request.paths, vec![directory.clone()]);
        }
        harness.screen.menu_on_ground = false;
        let Some(FileBrowserAction::File(request)) = harness.screen.run_menu_action(
            "files.open-terminal",
            &mut harness.state,
            Some(&directory),
            &entries,
        ) else {
            panic!("missing row terminal request");
        };
        assert_eq!(
            request.paths,
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
        );
        harness.screen.menu_on_ground = true;
        assert!(
            harness
                .screen
                .run_menu_action("files.open-terminal", &mut harness.state, None, &entries)
                .is_none()
        );
    }

    /// The three rows that are moves of the view rather than requests to a service reach the
    /// same state the toolbar's own controls do.
    #[test]
    fn the_menu_rows_that_change_the_view_do_what_the_toolbar_does() {
        let mut harness = Harness::new();
        harness.frame();
        let directory = PathBuf::from("/home/test/Downloads");
        let entries = harness.listing.entries.clone();

        assert_eq!(
            harness.screen.run_menu_action(
                "files.select-all",
                &mut harness.state,
                Some(&directory),
                &entries
            ),
            None,
            "selecting everything asks the host for nothing"
        );
        assert_eq!(harness.state.selected_paths().len(), entries.len());

        let before = harness.state.sort;
        harness.screen.run_menu_action(
            "files.sort",
            &mut harness.state,
            Some(&directory),
            &entries,
        );
        assert_ne!(harness.state.sort, before, "Sort by… moves the sort on");

        assert!(!harness.state.show_hidden);
        let reload = harness.screen.run_menu_action(
            "files.show-hidden",
            &mut harness.state,
            Some(&directory),
            &entries,
        );
        assert!(harness.state.show_hidden);
        assert_eq!(
            reload,
            Some(FileBrowserAction::Load {
                location: FilesLocation::Directory(directory),
                show_hidden: true,
            }),
            "the folder is re-read, because hidden files change what is in it"
        );
    }

    /// Secondary-click on a row selects it and opens its menu; choosing Move to Trash there
    /// makes the request the toolbar's Trash button makes, for that row.
    #[test]
    fn a_secondary_click_on_a_row_opens_its_menu_and_the_rows_act_on_it() {
        let mut harness = Harness::new();
        harness.frame();
        assert_eq!(harness.context_click(400.0, 210.0), None, "the menu opened");
        assert_eq!(
            harness.state.selected_paths(),
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")],
            "the menu is about the row it was opened over"
        );

        let index = menu_rows(true, true)
            .iter()
            .position(|id| *id == "files.trash")
            .unwrap();
        let trashed = verb_of(harness.click(534.0, 210.0 + 5.0 + 28.0 * index as f32 + 14.0));
        assert_eq!(
            trashed,
            Some((
                FileVerb::Trash,
                vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
            ))
        );
    }

    /// Secondary-click on the ground between rows opens the folder's menu instead, and a press
    /// outside it dismisses it without acting on anything underneath.
    #[test]
    fn a_secondary_click_on_the_ground_opens_the_folders_menu() {
        let mut harness = Harness::new();
        harness.frame();
        assert_eq!(harness.context_click(400.0, 400.0), None);
        let index = menu_rows(false, true)
            .iter()
            .position(|id| *id == "files.new-folder")
            .unwrap();
        let row = harness.menu_row(index);
        let asked = verb_of(harness.click((row.x + row.w / 2) as f32, (row.y + row.h / 2) as f32));
        assert_eq!(asked.map(|(verb, _)| verb), Some(FileVerb::NewFolder));
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

    /// The toolbar's location is a breadcrumb: each shown ancestor is a button, and pressing
    /// one goes there. With the 146px the fixture leaves the row, "/home/test/Downloads" keeps
    /// its last two segments — "test" is the clickable ancestor.
    #[test]
    fn toolbar_breadcrumb_navigates_to_an_ancestor() {
        let mut harness = Harness::new();
        harness.frame();
        harness.click(470.0, 29.0);
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test"))
        );
    }

    /// The Files button hands the open folder to the standalone manager — the route from a
    /// quick Home pane to the surface with the full drives sidebar.
    #[test]
    fn toolbar_files_button_opens_the_folder_in_the_manager() {
        let mut harness = Harness::new();
        harness.frame();
        assert_eq!(
            harness.click(416.0, 29.0),
            Some(FileBrowserAction::OpenInFiles(PathBuf::from(
                "/home/test/Downloads"
            )))
        );
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
        let clicked = verb_of(harness.click(260.0, 104.0));
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
        //
        // The first press selects and nothing else — that is what gives the file verbs a
        // selection to act on — and the second press on the selected row opens it.
        assert_eq!(harness.click(400.0, 172.0), None);
        assert_eq!(
            harness.state.selected_paths(),
            vec![PathBuf::from("/home/test/Downloads/Folder")]
        );
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test/Downloads"))
        );
        assert_eq!(
            harness.click(400.0, 172.0),
            Some(FileBrowserAction::Load {
                location: FilesLocation::Directory(PathBuf::from("/home/test/Downloads/Folder")),
                show_hidden: false,
            })
        );
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test/Downloads/Folder"))
        );
        // The harness keeps serving the Downloads listing, so the surface is now somewhere it
        // has no rows for; go back to where the rows are, and the same two presses apply.
        harness.state.go_back();
        harness.frame();
        assert_eq!(harness.click(400.0, 210.0), None);
        assert_eq!(
            harness.click(400.0, 210.0),
            Some(FileBrowserAction::Open(PathBuf::from(
                "/home/test/Downloads/paper.pdf"
            )))
        );
    }

    #[test]
    fn file_browser_keyboard_navigates_entries_roots_and_parent() {
        let mut harness = Harness::new();
        harness.input.key(KeyInput::Enter);
        harness.frame();
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test/Downloads/Folder"))
        );

        harness.input.key(KeyInput::Down);
        harness.input.key(KeyInput::Enter);
        assert!(matches!(
            harness.frame(),
            Some(FileBrowserAction::Open(path)) if path.ends_with("paper.pdf")
        ));

        harness.input.key(KeyInput::Backspace);
        harness.frame();
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test"))
        );

        harness.input.key(KeyInput::Left);
        harness.input.key(KeyInput::Enter);
        harness.frame();
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/home/test/Downloads"))
        );
    }

    /// The history the whole object is built on: three folders deep, Back walks out and Forward
    /// walks back in, and the surface never asks the host for anything to do it.
    #[test]
    fn grid_selects_before_opening_and_context_menu_targets_the_clicked_file() {
        let mut h = Harness::new();
        h.frame();
        if let Ok(dir) = std::env::var("LEGE_FILES_RENDER_DIR") {
            super::super::offscreen::write_png(&Path::new(&dir).join("files-list.png"), &h.buffer)
                .unwrap();
        }
        h.state.view = ViewMode::Thumbnails;
        h.frame();
        if let Ok(dir) = std::env::var("LEGE_FILES_RENDER_DIR") {
            super::super::offscreen::write_png(&Path::new(&dir).join("files-grid.png"), &h.buffer)
                .unwrap();
        }
        assert_eq!(h.click(400.0, 210.0), None);
        assert_eq!(
            h.state.selected_paths(),
            vec![h.listing.entries[1].path.clone()]
        );
        assert!(matches!(
            h.click(400.0, 210.0),
            Some(FileBrowserAction::Open(_))
        ));
        h.state.selection.clear();
        assert_eq!(h.context_click(400.0, 210.0), None);
        assert!(h.screen.menu.is_open());
        assert!(!h.screen.menu_on_ground);
        assert_eq!(
            h.state.selected_paths(),
            vec![h.listing.entries[1].path.clone()]
        );
    }

    #[test]
    fn grid_arrows_follow_rows_and_end_reveals_and_selects_the_last_file() {
        let mut h = Harness::new();
        let template = h.listing.entries[1].clone();
        h.listing.entries = (0..100)
            .map(|i| BrowseEntry {
                name: format!("file-{i:03}"),
                path: h.listing.directory.join(format!("file-{i:03}")),
                ..template.clone()
            })
            .collect();
        h.state.view = ViewMode::Thumbnails;
        h.frame();
        h.press(KeyInput::Right, Modifiers::default());
        assert_eq!(h.state.focus, 1);
        h.press(KeyInput::Down, Modifiers::default());
        assert_eq!(h.state.focus, 7, "six columns at this width");
        h.press(
            KeyInput::End,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(h.state.focus, 99);
        assert_eq!(h.state.selection.len(), 93);
        assert!(h.state.entry_scroll.offset() > 0);
        h.press(KeyInput::Home, Modifiers::default());
        assert_eq!(
            h.state.selected_paths(),
            vec![h.listing.entries[0].path.clone()]
        );
        assert_eq!(h.state.entry_scroll.offset(), 0);
    }

    #[test]
    fn alt_navigation_and_hidden_refresh_use_the_current_location() {
        let mut h = Harness::new();
        h.frame();
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        h.press(KeyInput::Up, alt);
        assert_eq!(h.here(), FilesLocation::Directory("/home/test".into()));
        // Harness supplies the original directory again; its history still contains Recent.
        h.press(KeyInput::Left, alt);
        assert_eq!(
            h.here(),
            FilesLocation::Directory(h.listing.directory.clone())
        );
        h.press(KeyInput::Right, alt);
        assert_eq!(
            h.here(),
            FilesLocation::Directory(h.listing.directory.clone())
        );
        assert!(matches!(
            h.press(KeyInput::Character('h'), control()),
            Some(FileBrowserAction::Load {
                show_hidden: true,
                ..
            })
        ));
        assert!(matches!(
            h.press(KeyInput::Function(5), Modifiers::default()),
            Some(FileBrowserAction::Load {
                show_hidden: true,
                ..
            })
        ));
    }

    #[test]
    fn navigation_history_walks_back_and_forward_without_a_service() {
        let mut harness = Harness::new();
        harness.frame();
        harness.state.go_to(FilesLocation::Directory("/a".into()));
        harness.state.go_to(FilesLocation::Directory("/a/b".into()));
        harness
            .state
            .go_to(FilesLocation::Directory("/a/b/c".into()));
        assert!(harness.state.go_back());
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/a/b"))
        );
        assert!(harness.state.go_forward());
        assert_eq!(
            harness.here(),
            FilesLocation::Directory(PathBuf::from("/a/b/c"))
        );
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
            harness.state.selected_paths(),
            vec![
                PathBuf::from("/home/test/Downloads/Folder"),
                PathBuf::from("/home/test/Downloads/paper.pdf"),
            ]
        );

        harness.input.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        harness.input.cursor_moved(400.0, 172.0);
        harness.input.mouse(MouseButton::Left, true);
        harness.input.mouse(MouseButton::Left, false);
        assert_eq!(harness.frame(), None);
        assert_eq!(
            harness.state.selected_paths(),
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
        );

        harness.input.key(KeyInput::Character('a'));
        assert_eq!(harness.frame(), None);
        assert_eq!(harness.state.selected_paths().len(), 2);
    }

    #[test]
    fn file_browser_search_results_use_the_same_clickable_rows() {
        let mut harness = Harness::new();
        let entries = harness.listing.entries.clone();
        harness.frame_with_search(Some("paper"), Some(&entries));
        // Select, then open: the same two presses a directory listing takes.
        harness.input.cursor_moved(400.0, 210.0);
        harness.input.mouse(MouseButton::Left, true);
        harness.input.mouse(MouseButton::Left, false);
        // The harness never serves a search listing, so every frame here also asks for one;
        // what matters is that the press selected rather than opened.
        assert!(matches!(
            harness.frame_with_search(Some("paper"), Some(&entries)),
            Some(FileBrowserAction::Load { .. })
        ));
        assert_eq!(
            harness.state.selected_paths(),
            vec![PathBuf::from("/home/test/Downloads/paper.pdf")]
        );
        harness.input.cursor_moved(400.0, 210.0);
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
        harness.state.focus = 1;
        harness
            .state
            .selection
            .replace(harness.listing.entries[1].path.clone());
        let selected_before = harness.state.selected_paths();
        let focus_before = harness.state.focus;

        harness.state.toggle_view();
        assert_eq!(harness.state.view, ViewMode::Thumbnails);
        harness.frame();
        assert_eq!(harness.state.selected_paths(), selected_before);
        assert_eq!(harness.state.focus, focus_before);

        harness.state.toggle_view();
        assert_eq!(harness.state.view, ViewMode::List);
        harness.frame();
        assert_eq!(harness.state.selected_paths(), selected_before);
        assert_eq!(harness.state.focus, focus_before);
    }

    /// Re-sorting uses the shared contract, keeps the path-keyed selection, and focus follows
    /// its entry to the new row.
    #[test]
    fn view_mode_sort_keeps_selection_and_focus_follows_its_entry() {
        let mut harness = Harness::new();
        harness.frame();
        // Focus the file (index 1 in default folder-first order), select it.
        harness.state.focus = 1;
        let file = harness.listing.entries[1].path.clone();
        harness.state.selection.replace(file.clone());

        harness.state.set_sort(
            SortDescriptor {
                field: SortField::NaturalName,
                ascending: false,
                folders_first: false,
            },
            &harness.listing.entries,
        );
        // Descending, folders no longer first: paper.pdf now leads.
        let ordered = FilesSurfaceState::sorted(&harness.listing.entries, harness.state.sort);
        assert_eq!(ordered[0].path, file);
        assert_eq!(harness.state.focus, 0, "focus followed paper.pdf");
        assert_eq!(harness.state.selected_paths(), vec![file]);
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
                .state
                .begin_thumbnails(&harness.listing.entries, &have)
                .is_empty(),
            "list mode requests nothing"
        );
        harness.state.view = ViewMode::Thumbnails;
        harness.frame();
        let requests = harness
            .state
            .begin_thumbnails(&harness.listing.entries, &have);
        assert!(!requests.is_empty(), "the visible cells are asked for");
        assert!(
            requests.len() <= FilesSurfaceState::THUMBNAIL_BATCH,
            "one bounded batch, however long the directory: {}",
            requests.len()
        );
        let generation = requests[0].0;
        assert!(harness.state.thumbnail_current(generation));

        let mut have = std::collections::HashMap::new();
        have.insert(requests[0].1.clone(), Bitmap::new(1, 1));
        assert!(
            !harness
                .state
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
            !harness.state.thumbnail_current(generation),
            "outstanding requests are from a dead generation"
        );
    }
}
