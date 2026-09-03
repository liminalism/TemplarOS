//! Where the Files object is, and everything it remembers about being there.
//!
//! One state, consumed at every rung. The tile reads it to say what it is showing; the expanded
//! surface reads and writes it. That is what makes expansion continuous: collapsing the object
//! and opening it again lands on the same directory, with the same rows selected and the same
//! scroll position, because nothing was ever thrown away.
//!
//! The navigation here is ported from `apps/lege-files`'s `BrowserModel` — deliberately, and it
//! is a duplication that is known and logged rather than an oversight. Two consumers is not
//! enough to justify a shared crate, and the shell must not depend on the application; when a
//! third appears, this is the pair to extract.
//!
//! Pure: no D-Bus, no async, no compositor. It says where it wants to be and the host fetches.

use std::path::{Path, PathBuf};

use lege_field::Selection;
use lege_types::file_sort::{SortDescriptor, SortField, SortItem, ViewMode, compare};
use pixelkit_ui::ScrollState;

use crate::file_browser::BrowseEntry;

/// One place the Files object can be.
///
/// A directory is the ordinary case and the only one with a path. The rest are the machine's own
/// answers to questions about files — what was used lately, what is mounted, what was thrown
/// away, what is being copied — and each is a location rather than a mode, so the back stack, the
/// breadcrumb and the pinned list all treat them the same way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesLocation {
    Directory(PathBuf),
    Recent,
    Drives,
    Trash,
    Operations,
    /// Results for `query`, rooted at `root`. A search is somewhere you *are*, so it goes on the
    /// back stack like anywhere else and leaving it is the ordinary Back.
    Search {
        root: PathBuf,
        query: String,
    },
}

impl FilesLocation {
    /// The directory this location lists, if it lists one.
    pub fn directory(&self) -> Option<&Path> {
        match self {
            Self::Directory(path) => Some(path),
            Self::Search { root, .. } => Some(root),
            _ => None,
        }
    }

    /// What the breadcrumb and the pinned row call it.
    pub fn label(&self) -> String {
        match self {
            Self::Directory(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            Self::Recent => "Recent".to_owned(),
            Self::Drives => "Drives".to_owned(),
            Self::Trash => "Trash".to_owned(),
            Self::Operations => "Operations".to_owned(),
            Self::Search { query, .. } => format!("Search · {query}"),
        }
    }

    /// Whether this location's rows come from a drawer feed rather than from `Files.Browse1`.
    pub fn feed(&self) -> Option<crate::home_surface::Drawer> {
        use crate::home_surface::Drawer;
        match self {
            Self::Recent => Some(Drawer::Recent),
            Self::Drives => Some(Drawer::Drives),
            Self::Trash => Some(Drawer::Trash),
            Self::Operations => Some(Drawer::Operations),
            _ => None,
        }
    }
}

/// What kind of thing a pinned location is, which is the only reason the sidebar draws two
/// different markers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinnedKind {
    Place,
    Drive,
    /// A question the machine answers about files, rather than a folder — Recent, Trash,
    /// Operations.
    Feed,
}

/// One row of the pinned list: somewhere worth reaching in a single press.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedLocation {
    pub label: String,
    /// What is worth saying about this row *beyond* its name — a volume's free space, or that it
    /// is not mounted. Empty for an ordinary folder: "Documents / Standard folder" said the same
    /// thing twice, once in a word the icon now carries (the fifth hands-on run).
    pub detail: String,
    pub location: FilesLocation,
    pub kind: PinnedKind,
}

impl PinnedLocation {
    /// The freedesktop icon name this row is drawn with. `home` is the user's home directory,
    /// which is the one folder whose icon cannot be told from its name alone.
    pub fn icon_name(&self, home: Option<&Path>) -> &'static str {
        match &self.location {
            FilesLocation::Recent => "document-open-recent",
            FilesLocation::Trash => "user-trash",
            FilesLocation::Operations => "edit-copy",
            FilesLocation::Drives => "drive-harddisk",
            FilesLocation::Search { .. } => "system-search",
            FilesLocation::Directory(path) => {
                if self.kind == PinnedKind::Drive {
                    "drive-harddisk"
                } else {
                    crate::icons::directory_icon_name(path, home)
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusArea {
    Pinned,
    Entries,
}

/// Everything the Files object remembers, at every rung and across every collapse.
#[derive(Debug)]
pub struct FilesSurfaceState {
    location: FilesLocation,
    back: Vec<FilesLocation>,
    forward: Vec<FilesLocation>,
    pub selection: Selection<PathBuf>,
    pub focus: usize,
    /// The row a shift-extended selection grows from.
    pub anchor: Option<PathBuf>,
    pub focus_area: FocusArea,
    pub pinned_focus: usize,
    pub sort: SortDescriptor,
    pub view: ViewMode,
    pub show_hidden: bool,
    /// What is typed into the search control while this surface is scoped to its location.
    pub query: String,
    pub entry_scroll: ScrollState,
    pub pinned_scroll: ScrollState,
    /// Bumped whenever the listing changes, so a thumbnail decoded for the old one is
    /// recognisably stale and dropped by the host instead of painted into the wrong folder.
    thumbnail_generation: u64,
    /// The entry range the last thumbnail draw could see, plus a row of lookahead — the only
    /// entries [`Self::begin_thumbnails`] will ask for.
    visible_thumbnails: std::ops::Range<usize>,
    /// The directory the current selection and focus belong to, so arriving somewhere new
    /// clears them and merely re-reading the same place does not.
    last_directory: Option<PathBuf>,
}

impl Default for FilesSurfaceState {
    fn default() -> Self {
        Self {
            location: FilesLocation::Recent,
            back: Vec::new(),
            forward: Vec::new(),
            selection: Selection::default(),
            focus: 0,
            anchor: None,
            focus_area: FocusArea::Entries,
            pinned_focus: 0,
            sort: SortDescriptor::default(),
            view: ViewMode::default(),
            show_hidden: false,
            query: String::new(),
            entry_scroll: ScrollState::new(),
            pinned_scroll: ScrollState::new(),
            thumbnail_generation: 0,
            visible_thumbnails: 0..0,
            last_directory: None,
        }
    }
}

impl FilesSurfaceState {
    pub fn location(&self) -> &FilesLocation {
        &self.location
    }

    pub fn directory(&self) -> Option<&Path> {
        self.location.directory()
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Go somewhere new. The place being left joins the back stack and the forward stack is
    /// discarded — the ordinary rule everywhere, and the reason Forward after a detour does not
    /// silently take you somewhere you never were.
    ///
    /// Navigating to where you already are does nothing at all, so a double press on a pinned
    /// row does not fill the history with one place repeated.
    pub fn go_to(&mut self, to: FilesLocation) {
        if to == self.location {
            return;
        }
        self.forward.clear();
        let from = std::mem::replace(&mut self.location, to);
        self.back.push(from);
    }

    pub fn go_back(&mut self) -> bool {
        let Some(to) = self.back.pop() else {
            return false;
        };
        let from = std::mem::replace(&mut self.location, to);
        self.forward.push(from);
        true
    }

    pub fn go_forward(&mut self) -> bool {
        let Some(to) = self.forward.pop() else {
            return false;
        };
        let from = std::mem::replace(&mut self.location, to);
        self.back.push(from);
        true
    }

    /// Up one level. Only a directory has a parent; a feed location is already the top of its
    /// own tree, and `/` is the top of the filesystem.
    pub fn go_up(&mut self, parent: Option<&Path>) -> bool {
        let Some(parent) = parent.or_else(|| self.directory().and_then(Path::parent)) else {
            return false;
        };
        let to = FilesLocation::Directory(parent.to_path_buf());
        if to == self.location {
            return false;
        }
        self.go_to(to);
        true
    }

    /// Replace where we are without touching the history: what a refresh, a hidden-files toggle
    /// or a search over the same root does. Going Back from a search still leaves the search.
    pub fn replace(&mut self, to: FilesLocation) {
        self.location = to;
    }

    /// Search from here. Typing a query is a move, so it stacks; clearing it goes back to the
    /// directory rather than to a blank.
    pub fn search(&mut self, query: &str) {
        let root = self
            .directory()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        if query.trim().is_empty() {
            self.go_to(FilesLocation::Directory(root));
        } else {
            self.go_to(FilesLocation::Search {
                root,
                query: query.to_owned(),
            });
        }
    }

    /// Note that this listing is what is on screen now, clearing selection and focus only when
    /// it is a genuinely different place. Re-reading the same directory keeps everything.
    pub fn observe(&mut self, directory: Option<&Path>) {
        let directory = directory.map(Path::to_path_buf);
        if directory == self.last_directory {
            return;
        }
        self.last_directory = directory;
        self.focus = 0;
        self.anchor = None;
        self.selection.clear();
        self.entry_scroll = ScrollState::new();
        self.thumbnail_generation = self.thumbnail_generation.wrapping_add(1);
        self.visible_thumbnails = 0..0;
    }

    /// Drop selected paths that are no longer on screen.
    pub fn retain_selection(&mut self, entries: &[BrowseEntry]) {
        self.selection
            .retain(|path| entries.iter().any(|entry| &entry.path == path));
    }

    /// Move focus to `index` and take the selection with it, extending from the anchor when the
    /// shift modifier is held. Ported from `BrowserModel`'s keyboard selection, which is the
    /// behaviour the Files application already has.
    pub fn focus_entry(&mut self, index: usize, entries: &[BrowseEntry], extend: bool) {
        let previous = self.focus;
        self.focus = index.min(entries.len().saturating_sub(1));
        let Some(path) = entries.get(self.focus).map(|entry| entry.path.clone()) else {
            return;
        };
        let order: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        if extend {
            if self.selection.is_empty()
                && let Some(anchor) = entries.get(previous)
            {
                self.anchor = Some(anchor.path.clone());
                self.selection.replace(anchor.path.clone());
            }
            self.selection.extend_to(&order, path);
        } else {
            self.anchor = Some(path.clone());
            self.selection.replace(path);
        }
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selection.to_vec()
    }

    /// Entries under the shared ordering contract. The default descriptor is exactly what the
    /// service already served, so this only runs for a non-default choice.
    pub fn sorted(entries: &[BrowseEntry], sort: SortDescriptor) -> Vec<BrowseEntry> {
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

    /// Change the ordering. The path-keyed selection is untouched, and focus follows the entry
    /// it was on to its new row — `entries` is the order focus currently indexes into.
    pub fn set_sort(&mut self, sort: SortDescriptor, entries: &[BrowseEntry]) {
        let current = if self.sort == SortDescriptor::default() {
            entries.to_vec()
        } else {
            Self::sorted(entries, self.sort)
        };
        let focused = current.get(self.focus).map(|entry| entry.path.clone());
        self.sort = sort;
        if let Some(focused) = focused {
            let next = if sort == SortDescriptor::default() {
                entries.to_vec()
            } else {
                Self::sorted(entries, sort)
            };
            if let Some(index) = next.iter().position(|entry| entry.path == focused) {
                self.focus = index;
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

    pub fn thumbnail_generation(&self) -> u64 {
        self.thumbnail_generation
    }

    /// Remember which entries the last draw could see, with a row of lookahead in each
    /// direction so a slow scroll never shows a beat of empty cells.
    pub fn saw_thumbnails(&mut self, range: std::ops::Range<usize>) {
        self.visible_thumbnails = range;
    }

    /// Bounded thumbnail work for the host: only entries the last draw could see, only files,
    /// only ones not already decoded, and at most [`Self::THUMBNAIL_BATCH`] per call. Decoding
    /// happens in the host's async task — never here, never on the render thread.
    pub fn begin_thumbnails(
        &self,
        entries: &[BrowseEntry],
        have: &std::collections::HashMap<PathBuf, pixelkit_raster::Bitmap>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str) -> FilesLocation {
        FilesLocation::Directory(PathBuf::from(path))
    }

    /// The history is the ordinary one: going somewhere stacks what you left, Back and Forward
    /// walk it, and a detour from the middle discards what was ahead.
    #[test]
    fn navigation_stacks_back_and_forward_and_a_detour_discards_the_future() {
        let mut state = FilesSurfaceState::default();
        assert!(!state.can_back() && !state.can_forward());

        state.go_to(dir("/a"));
        state.go_to(dir("/a/b"));
        state.go_to(dir("/a/b/c"));
        assert_eq!(state.location(), &dir("/a/b/c"));

        assert!(state.go_back());
        assert_eq!(state.location(), &dir("/a/b"));
        assert!(state.can_forward());
        assert!(state.go_forward());
        assert_eq!(state.location(), &dir("/a/b/c"));

        state.go_back();
        state.go_to(dir("/elsewhere"));
        assert!(
            !state.can_forward(),
            "a detour left a forward step that was never taken"
        );
        assert!(state.go_back());
        assert_eq!(state.location(), &dir("/a/b"));
    }

    /// Going where you already are is not a move, so pressing a pinned row twice does not fill
    /// the history with one place repeated.
    #[test]
    fn arriving_where_you_already_are_is_not_a_move() {
        let mut state = FilesSurfaceState::default();
        state.go_to(dir("/a"));
        state.go_to(dir("/a"));
        assert!(state.go_back());
        assert_eq!(state.location(), &FilesLocation::Recent);
        assert!(!state.go_back());
    }

    /// A search is somewhere you are: it stacks, and leaving it is the ordinary Back. Clearing
    /// the query returns to the directory it was rooted at.
    #[test]
    fn a_search_is_a_location_and_clearing_it_returns_to_its_root() {
        let mut state = FilesSurfaceState::default();
        state.go_to(dir("/a/b"));
        state.search("report");
        assert_eq!(
            state.location(),
            &FilesLocation::Search {
                root: PathBuf::from("/a/b"),
                query: "report".to_owned()
            }
        );
        assert_eq!(state.directory(), Some(Path::new("/a/b")));
        state.search("  ");
        assert_eq!(state.location(), &dir("/a/b"));
    }

    /// Up walks the tree and stops at its top rather than looping.
    #[test]
    fn up_walks_to_the_parent_and_stops_at_the_root() {
        let mut state = FilesSurfaceState::default();
        state.go_to(dir("/a/b"));
        assert!(state.go_up(None));
        assert_eq!(state.location(), &dir("/a"));
        assert!(state.go_up(None));
        assert_eq!(state.location(), &dir("/"));
        assert!(!state.go_up(None), "/ has no parent");
    }

    /// Selection and focus survive a re-read of the same directory and are dropped on arriving
    /// somewhere else — the difference between a refresh and a move.
    #[test]
    fn observing_the_same_directory_keeps_the_selection_and_a_new_one_clears_it() {
        let mut state = FilesSurfaceState::default();
        state.observe(Some(Path::new("/a")));
        state.selection.replace(PathBuf::from("/a/one"));
        state.focus = 3;

        state.observe(Some(Path::new("/a")));
        assert_eq!(state.selected_paths(), vec![PathBuf::from("/a/one")]);
        assert_eq!(state.focus, 3);

        state.observe(Some(Path::new("/b")));
        assert!(state.selected_paths().is_empty());
        assert_eq!(state.focus, 0);
    }

    /// The whole point of the state living outside the view: expand, walk three folders deep,
    /// select something, collapse, expand again — and be exactly where you were. Nothing here
    /// resets, so there is nothing for a collapse to throw away.
    #[test]
    fn expanding_again_lands_exactly_where_the_object_was_left() {
        let mut state = FilesSurfaceState::default();
        state.go_to(dir("/home/a"));
        state.observe(Some(Path::new("/home/a")));
        state.go_to(dir("/home/a/work"));
        state.observe(Some(Path::new("/home/a/work")));
        state.go_to(dir("/home/a/work/2026"));
        state.observe(Some(Path::new("/home/a/work/2026")));
        state
            .selection
            .replace(PathBuf::from("/home/a/work/2026/notes.md"));
        state.focus = 4;

        // A collapse is the field drawing something else for a while; the state is untouched by
        // it, and the next expansion re-reads the same directory.
        state.observe(Some(Path::new("/home/a/work/2026")));

        assert_eq!(state.location(), &dir("/home/a/work/2026"));
        assert_eq!(
            state.selected_paths(),
            vec![PathBuf::from("/home/a/work/2026/notes.md")]
        );
        assert_eq!(state.focus, 4);
        assert!(state.go_back());
        assert_eq!(state.location(), &dir("/home/a/work"));
    }

    #[test]
    fn a_feed_location_names_its_drawer_and_a_directory_does_not() {
        use crate::home_surface::Drawer;
        assert_eq!(FilesLocation::Trash.feed(), Some(Drawer::Trash));
        assert_eq!(FilesLocation::Recent.feed(), Some(Drawer::Recent));
        assert_eq!(dir("/a").feed(), None);
        assert_eq!(dir("/a/notes").label(), "notes");
    }
}
