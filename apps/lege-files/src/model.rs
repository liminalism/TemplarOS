//! Pure file-manager state: stale-safe browsing, history, selection and operation intent.

use crate::actions::ActionId;
use lege_client::files::{
    BrowseEntry, BrowserSources, DirectoryListing, FileInfo, OperationOptions, OperationRecord,
    OperationRequest, PreviewMetadata, TrashEntry, VolumeInfo, valid_file_name,
};
use pixelkit_raster::Bitmap;
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SelectionModifiers {
    pub control: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardMode {
    Copy,
    Move,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub mode: ClipboardMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowseSnapshot {
    pub listing: DirectoryListing,
    pub volume: VolumeInfo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertiesSnapshot {
    pub info: FileInfo,
    pub volume: VolumeInfo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewSnapshot {
    pub metadata: PreviewMetadata,
    pub bitmap: Bitmap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowseRequest {
    pub id: u64,
    pub path: PathBuf,
    pub show_hidden: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectRequest {
    pub id: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewRequest {
    pub id: u64,
    pub path: PathBuf,
}

/// One bounded thumbnail decode, tagged with the listing generation it was asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailRequest {
    pub generation: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedOperation {
    pub request: OperationRequest,
    pub options: OperationOptions,
}

/// The trash as a place: a listing that is not a directory, so it is held apart from `listing`
/// rather than dressed up as one. It has no parent, no hidden files and no paste.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrashView {
    pub entries: Vec<TrashEntry>,
    /// Why the listing could not be read, if it could not.
    pub error: Option<String>,
    pub loading: bool,
}

/// What the trash is being asked to do, once any confirmation has been answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrashIntent {
    Restore(Vec<PathBuf>),
    DeletePermanently(Vec<PathBuf>),
    Empty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Navigation {
    Reset,
    Push,
    Back,
    Forward,
    Replace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingBrowse {
    request: BrowseRequest,
    navigation: Navigation,
}

#[derive(Debug)]
pub struct BrowserModel {
    pub sources: BrowserSources,
    pub listing: Option<DirectoryListing>,
    /// The one shared ordering contract (`lege_types::file_sort`): the listing is kept sorted
    /// by this at all times, so rows, selection indices and keyboard focus always agree.
    pub sort: lege_types::file_sort::SortDescriptor,
    /// List columns or thumbnail cells. Switching preserves selection and focus.
    pub view: lege_types::file_sort::ViewMode,
    /// Thumbnail bitmaps by path, filled boundedly by `begin_thumbnails` — only visible and
    /// near-visible cells are ever requested, and stale generations are dropped.
    pub thumbnails: std::collections::HashMap<PathBuf, Bitmap>,
    pub volume: Option<VolumeInfo>,
    pub show_hidden: bool,
    pub selected: BTreeSet<PathBuf>,
    pub focus: usize,
    pub clipboard: Option<Clipboard>,
    pub properties: Option<PropertiesSnapshot>,
    pub properties_error: Option<String>,
    pub preview: Option<PreviewSnapshot>,
    pub preview_error: Option<String>,
    pub operation: Option<OperationRecord>,
    pub operation_summary: Option<String>,
    pub active_operation_id: Option<String>,
    pub undo_id: Option<String>,
    pub status: Option<String>,
    pub error: Option<String>,
    /// The trash, when the trash is the open place. `None` means a directory is.
    pub trash: Option<TrashView>,
    /// A destructive action that has been asked for once and is waiting to be asked again.
    ///
    /// The rule is a second press of the *same* action rather than a timer: this model is pure
    /// and has no clock, and anything else the user does clears it — which is the behaviour that
    /// matters, since the danger is a confirmation left armed while attention moved on.
    pub confirm: Option<ActionId>,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    anchor: Option<usize>,
    sequence: u64,
    inspect_sequence: u64,
    preview_sequence: u64,
    pending: Option<PendingBrowse>,
    pending_inspect: Option<InspectRequest>,
    pending_preview: Option<PreviewRequest>,
    requested_initial: Option<PathBuf>,
    thumbnail_generation: u64,
}

impl BrowserModel {
    pub fn new(requested_initial: Option<PathBuf>, show_hidden: bool) -> Self {
        Self {
            sources: BrowserSources::default(),
            listing: None,
            sort: lege_types::file_sort::SortDescriptor::default(),
            view: lege_types::file_sort::ViewMode::default(),
            thumbnails: std::collections::HashMap::new(),
            volume: None,
            show_hidden,
            selected: BTreeSet::new(),
            focus: 0,
            clipboard: None,
            properties: None,
            properties_error: None,
            preview: None,
            preview_error: None,
            operation: None,
            operation_summary: None,
            active_operation_id: None,
            undo_id: None,
            status: Some("Connecting to the file service…".into()),
            error: None,
            trash: None,
            confirm: None,
            back: Vec::new(),
            forward: Vec::new(),
            anchor: None,
            sequence: 0,
            inspect_sequence: 0,
            preview_sequence: 0,
            pending: None,
            pending_inspect: None,
            pending_preview: None,
            requested_initial,
            thumbnail_generation: 0,
        }
    }

    pub fn directory(&self) -> Option<&std::path::Path> {
        self.listing
            .as_ref()
            .map(|listing| listing.directory.as_path())
    }

    pub fn pending_path(&self) -> Option<&std::path::Path> {
        self.pending
            .as_ref()
            .map(|pending| pending.request.path.as_path())
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty() && self.pending.is_none()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty() && self.pending.is_none()
    }

    pub fn set_sources(&mut self, sources: BrowserSources) -> Option<BrowseRequest> {
        self.sources = sources;
        if self.listing.is_some() || self.pending.is_some() {
            return None;
        }
        let start = self
            .requested_initial
            .take()
            .or_else(|| self.sources.home().map(ToOwned::to_owned))
            .or_else(|| self.sources.first_available().map(ToOwned::to_owned));
        if let Some(path) = start {
            Some(self.begin(path, Navigation::Reset))
        } else {
            self.status = None;
            self.error = Some("No available Home folder or mounted drive.".into());
            None
        }
    }

    pub fn navigate(&mut self, path: PathBuf) -> BrowseRequest {
        self.begin(path, Navigation::Push)
    }

    pub fn refresh(&mut self) -> Option<BrowseRequest> {
        self.directory()
            .map(ToOwned::to_owned)
            .map(|path| self.begin(path, Navigation::Replace))
    }

    pub fn toggle_hidden(&mut self) -> Option<BrowseRequest> {
        self.show_hidden = !self.show_hidden;
        self.refresh()
    }

    pub fn up(&mut self) -> Option<BrowseRequest> {
        self.listing
            .as_ref()
            .and_then(|listing| listing.parent.clone())
            .map(|path| self.begin(path, Navigation::Push))
    }

    pub fn back(&mut self) -> Option<BrowseRequest> {
        self.back
            .last()
            .cloned()
            .map(|path| self.begin(path, Navigation::Back))
    }

    pub fn forward(&mut self) -> Option<BrowseRequest> {
        self.forward
            .last()
            .cloned()
            .map(|path| self.begin(path, Navigation::Forward))
    }

    fn begin(&mut self, path: PathBuf, navigation: Navigation) -> BrowseRequest {
        // Browsing a directory leaves the trash: they are two different places, and one listing
        // is shown at a time.
        self.trash = None;
        self.confirm = None;
        self.sequence = self.sequence.wrapping_add(1);
        let request = BrowseRequest {
            id: self.sequence,
            path,
            show_hidden: self.show_hidden,
        };
        self.pending = Some(PendingBrowse {
            request: request.clone(),
            navigation,
        });
        self.error = None;
        self.status = Some(format!("Loading {}…", request.path.display()));
        request
    }

    pub fn apply_browse(
        &mut self,
        id: u64,
        path: &std::path::Path,
        result: Result<BrowseSnapshot, String>,
    ) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.request.id != id || pending.request.path != path {
            return false;
        }
        let navigation = pending.navigation;
        self.pending = None;
        match result {
            Ok(snapshot) => {
                let old = self.directory().map(ToOwned::to_owned);
                match navigation {
                    Navigation::Reset => {
                        self.back.clear();
                        self.forward.clear();
                    }
                    Navigation::Push => {
                        if let Some(old) = old.filter(|old| *old != snapshot.listing.directory) {
                            self.back.push(old);
                            self.forward.clear();
                        }
                    }
                    Navigation::Back => {
                        self.back.pop();
                        if let Some(old) = old {
                            self.forward.push(old);
                        }
                    }
                    Navigation::Forward => {
                        self.forward.pop();
                        if let Some(old) = old {
                            self.back.push(old);
                        }
                    }
                    Navigation::Replace => {}
                }
                let mut listing = snapshot.listing;
                Self::sort_entries(&mut listing, self.sort);
                self.thumbnails.clear();
                self.thumbnail_generation = self.thumbnail_generation.wrapping_add(1);
                self.listing = Some(listing);
                self.volume = Some(snapshot.volume);
                self.selected.clear();
                self.anchor = None;
                self.focus = 0;
                self.properties = None;
                self.properties_error = None;
                self.preview = None;
                self.preview_error = None;
                self.pending_inspect = None;
                self.pending_preview = None;
                self.error = None;
                self.status = None;
                true
            }
            Err(error) => {
                self.error = Some(error.clone());
                self.status = Some(format!("Could not open {}", path.display()));
                false
            }
        }
    }

    /// The path of each visible row, in order: a trash listing's items when the trash is open,
    /// the directory's entries otherwise. Selection, focus and select-all are all written against
    /// this, so the trash is a place with the same keyboard behaviour rather than a special case
    /// with none.
    fn row_paths(&self) -> Vec<PathBuf> {
        match &self.trash {
            Some(view) => view
                .entries
                .iter()
                .map(|entry| entry.trashed_path.clone())
                .collect(),
            None => self.listing.as_ref().map_or_else(Vec::new, |listing| {
                listing
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect()
            }),
        }
    }

    /// Apply the shared comparator. The service already serves the default order; any other
    /// descriptor re-sorts locally with the very same contract, so Home, Files and the service
    /// can never disagree about the same directory.
    fn sort_entries(listing: &mut DirectoryListing, sort: lege_types::file_sort::SortDescriptor) {
        listing.entries.sort_by(|a, b| {
            lege_types::file_sort::compare(&Self::sort_item(a), &Self::sort_item(b), sort)
        });
    }

    fn sort_item(entry: &BrowseEntry) -> lege_types::file_sort::SortItem<'_> {
        lege_types::file_sort::SortItem {
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

    /// Change the sort descriptor: the listing re-orders in place, the selected path set is
    /// untouched, and keyboard focus follows the item it was on to its new row.
    pub fn set_sort(&mut self, sort: lege_types::file_sort::SortDescriptor) {
        self.sort = sort;
        let focused = self.focused_path().cloned();
        if let Some(listing) = &mut self.listing {
            Self::sort_entries(listing, sort);
        }
        if let Some(focused) = focused {
            let rows = self.row_paths();
            if let Some(index) = rows.iter().position(|path| *path == focused) {
                self.focus = index;
            }
        }
    }

    /// Switch between the list and thumbnail presentations. A view is how the same rows are
    /// drawn: selection, focus and the anchor stay exactly as they are, so toggling back and
    /// forth loses nothing.
    pub fn set_view(&mut self, view: lege_types::file_sort::ViewMode) {
        self.view = view;
    }

    /// The most thumbnail decodes one call may put in flight. Scrolling calls again with the
    /// new window; a bounded batch per frame is what keeps a huge directory from flooding the
    /// preview service.
    pub const THUMBNAIL_BATCH: usize = 16;

    /// Request thumbnails for the visible (plus one row of lookahead) file entries that have
    /// none yet — bounded, cancellable, and never decoding anything itself: the returned
    /// requests are handed to the same off-thread preview backend the inspector uses, and a
    /// response from before the last listing change is dropped by generation.
    pub fn begin_thumbnails(&mut self, visible: std::ops::Range<usize>) -> Vec<ThumbnailRequest> {
        if self.view != lege_types::file_sort::ViewMode::Thumbnails {
            return Vec::new();
        }
        let Some(listing) = &self.listing else {
            return Vec::new();
        };
        let end = visible.end.min(listing.entries.len());
        let start = visible.start.min(end);
        listing.entries[start..end]
            .iter()
            .filter(|entry| !entry.is_dir())
            .filter(|entry| !self.thumbnails.contains_key(&entry.path))
            .take(Self::THUMBNAIL_BATCH)
            .map(|entry| ThumbnailRequest {
                generation: self.thumbnail_generation,
                path: entry.path.clone(),
            })
            .collect()
    }

    /// A thumbnail arrived. A response from a superseded generation — the listing changed under
    /// it — is dropped, so a slow decode can never paint into the wrong directory.
    pub fn apply_thumbnail(
        &mut self,
        generation: u64,
        path: &std::path::Path,
        bitmap: Option<Bitmap>,
    ) -> bool {
        if generation != self.thumbnail_generation {
            return false;
        }
        if let Some(bitmap) = bitmap {
            self.thumbnails.insert(path.to_path_buf(), bitmap);
            return true;
        }
        false
    }

    pub fn select(&mut self, index: usize, modifiers: SelectionModifiers) -> bool {
        let entries = self.row_paths();
        let Some(path) = entries.get(index).cloned() else {
            return false;
        };
        let was_only_selected = self.selected.len() == 1 && self.selected.contains(&path);
        self.focus = index;
        if modifiers.shift {
            let anchor = self.anchor.unwrap_or(index);
            let (start, end) = if anchor <= index {
                (anchor, index)
            } else {
                (index, anchor)
            };
            if !modifiers.control {
                self.selected.clear();
            }
            self.selected.extend(entries[start..=end].iter().cloned());
        } else if modifiers.control {
            if !self.selected.remove(&path) {
                self.selected.insert(path);
            }
            self.anchor = Some(index);
        } else {
            self.selected.clear();
            self.selected.insert(path);
            self.anchor = Some(index);
        }
        self.properties = None;
        self.properties_error = None;
        self.pending_inspect = None;
        was_only_selected && !modifiers.control && !modifiers.shift
    }

    pub fn move_focus(&mut self, target: usize, extend: bool) {
        let len = self.row_paths().len();
        if len == 0 {
            return;
        }
        let index = target.min(len - 1);
        self.select(
            index,
            SelectionModifiers {
                shift: extend,
                control: false,
            },
        );
    }

    pub fn select_all(&mut self) {
        let rows = self.row_paths();
        if rows.is_empty() {
            return;
        }
        self.anchor = Some(0);
        self.selected = rows.into_iter().collect();
        self.properties = None;
        self.properties_error = None;
        self.pending_inspect = None;
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.properties = None;
        self.properties_error = None;
        self.preview = None;
        self.preview_error = None;
        self.pending_inspect = None;
        self.pending_preview = None;
    }

    pub fn focused_path(&self) -> Option<&PathBuf> {
        let listing = self.listing.as_ref()?;
        let entry = listing.entries.get(self.focus)?;
        self.selected.contains(&entry.path).then_some(&entry.path)
    }

    pub fn in_trash(&self) -> bool {
        self.trash.is_some()
    }

    pub fn trash_entries(&self) -> &[TrashEntry] {
        self.trash
            .as_ref()
            .map_or(&[] as &[TrashEntry], |view| view.entries.as_slice())
    }

    /// Open the trash. The caller asks the service for the listing; this only says which place
    /// is showing, so the pane never renders a directory's rows under the trash's columns.
    pub fn open_trash(&mut self) {
        self.clear_selection();
        self.confirm = None;
        self.trash = Some(TrashView {
            loading: true,
            ..TrashView::default()
        });
        self.error = None;
        self.status = Some("Loading the trash…".into());
    }

    /// A `Trash1.List` answer. Ignored when the trash is no longer the open place, so a listing
    /// that arrives after the user has navigated away cannot replace the directory they moved to.
    pub fn apply_trash(&mut self, result: Result<Vec<TrashEntry>, String>) {
        let Some(view) = self.trash.as_mut() else {
            return;
        };
        view.loading = false;
        match result {
            Ok(entries) => {
                // Anything that is no longer there cannot stay selected: the buttons above act
                // on the selection, and offering to restore a vanished item is a lie.
                let present: BTreeSet<PathBuf> = entries
                    .iter()
                    .map(|entry| entry.trashed_path.clone())
                    .collect();
                self.selected.retain(|path| present.contains(path));
                let count = entries.len();
                view.entries = entries;
                view.error = None;
                self.error = None;
                self.status = Some(match count {
                    0 => "The trash is empty.".to_owned(),
                    1 => "1 item in the trash.".to_owned(),
                    count => format!("{count} items in the trash."),
                });
            }
            Err(error) => {
                view.error = Some(error.clone());
                self.status = None;
                self.error = Some(error);
            }
        }
    }

    /// The trash items the selection names, in listing order.
    pub fn selected_trashed(&self) -> Vec<PathBuf> {
        self.trash_entries()
            .iter()
            .map(|entry| entry.trashed_path.clone())
            .filter(|path| self.selected.contains(path))
            .collect()
    }

    /// Ask for one trash action. `Ok(None)` means it needs confirming first and the prompt is in
    /// [`Self::status`]; the same action asked again goes through.
    ///
    /// Every destructive route funnels through here — button, menu row and accelerator alike — so
    /// none of them can be the one that skips the question.
    pub fn request_trash_action(
        &mut self,
        action: ActionId,
    ) -> Result<Option<TrashIntent>, String> {
        let armed = self.confirm == Some(action);
        self.confirm = None;
        let intent = match action {
            ActionId::Restore => {
                let paths = self.selected_trashed();
                if paths.is_empty() {
                    return Err("Select something in the trash to restore.".into());
                }
                TrashIntent::Restore(paths)
            }
            ActionId::DeletePermanently => {
                let paths = self.selected_trashed();
                if paths.is_empty() {
                    return Err("Select something in the trash to delete.".into());
                }
                TrashIntent::DeletePermanently(paths)
            }
            ActionId::EmptyTrash => {
                if self.trash_entries().is_empty() {
                    return Err("The trash is already empty.".into());
                }
                TrashIntent::Empty
            }
            other => return Err(format!("{} is not a trash action.", other.label())),
        };
        if action.destructive() && !armed {
            self.confirm = Some(action);
            self.error = None;
            self.status = Some(match &intent {
                TrashIntent::Empty => format!(
                    "Delete all {} item(s) in the trash for good? This cannot be undone — press                      Empty trash again to confirm.",
                    self.trash_entries().len()
                ),
                TrashIntent::DeletePermanently(paths) => format!(
                    "Delete {} item(s) for good? This cannot be undone — press Delete permanently                      again to confirm.",
                    paths.len()
                ),
                TrashIntent::Restore(_) => unreachable!("restoring is not destructive"),
            });
            return Ok(None);
        }
        Ok(Some(intent))
    }

    /// Forget a half-asked confirmation. Anything the user does other than repeating the action
    /// calls this, so a question that was walked away from cannot be answered by accident later.
    pub fn cancel_confirmation(&mut self) {
        self.confirm = None;
    }

    pub fn single_selected(&self) -> Option<&PathBuf> {
        (self.selected.len() == 1)
            .then(|| self.selected.iter().next())
            .flatten()
    }

    pub fn begin_inspect(&mut self) -> Option<InspectRequest> {
        let path = self.single_selected()?.clone();
        self.inspect_sequence = self.inspect_sequence.wrapping_add(1);
        let request = InspectRequest {
            id: self.inspect_sequence,
            path,
        };
        self.pending_inspect = Some(request.clone());
        self.properties = None;
        self.properties_error = None;
        self.status = Some("Calculating properties…".into());
        Some(request)
    }

    pub fn apply_inspect(
        &mut self,
        id: u64,
        path: &std::path::Path,
        result: Result<PropertiesSnapshot, String>,
    ) -> bool {
        if self
            .pending_inspect
            .as_ref()
            .is_none_or(|request| request.id != id || request.path != path)
        {
            return false;
        }
        self.pending_inspect = None;
        match result {
            Ok(snapshot) => {
                self.properties = Some(snapshot);
                self.properties_error = None;
                self.status = Some("Properties ready".into());
            }
            Err(error) => {
                self.properties_error = Some(error);
                self.status = Some("Could not calculate properties".into());
            }
        }
        true
    }

    pub fn begin_preview(&mut self) -> Option<PreviewRequest> {
        self.preview = None;
        self.preview_error = None;
        self.pending_preview = None;
        let path = self.single_selected()?.clone();
        self.preview_sequence = self.preview_sequence.wrapping_add(1);
        let request = PreviewRequest {
            id: self.preview_sequence,
            path,
        };
        self.pending_preview = Some(request.clone());
        Some(request)
    }

    pub fn apply_preview(
        &mut self,
        id: u64,
        path: &std::path::Path,
        result: Result<Option<PreviewSnapshot>, String>,
    ) -> bool {
        if self
            .pending_preview
            .as_ref()
            .is_none_or(|request| request.id != id || request.path != path)
        {
            return false;
        }
        self.pending_preview = None;
        match result {
            Ok(preview) => {
                self.preview = preview;
                self.preview_error = None;
            }
            Err(error) => {
                self.preview = None;
                self.preview_error = Some(error);
            }
        }
        true
    }

    /// The selection as a `text/uri-list` payload, for the clipboard and for a drag out.
    ///
    /// The same encoding Home uses (`lege_types::uri_list`), which is the whole point: what the
    /// Files app copies is what Home pastes and what any GTK or Qt application pastes, because
    /// there is one encoder rather than one per surface.
    pub fn selection_as_uri_list(&self) -> String {
        lege_types::uri_list::encode(&self.selected.iter().cloned().collect::<Vec<_>>())
    }

    /// Take a `text/uri-list` from elsewhere as the clipboard contents.
    ///
    /// A copy, never a move: a bare uri-list carries no cut/copy distinction, and guessing "move"
    /// would take somebody's original away from wherever it was needed.
    pub fn set_clipboard_from_uri_list(&mut self, payload: &str) -> bool {
        let paths = lege_types::uri_list::decode(payload);
        if paths.is_empty() {
            return false;
        }
        self.clipboard = Some(Clipboard {
            paths,
            mode: ClipboardMode::Copy,
        });
        true
    }

    pub fn set_clipboard(&mut self, mode: ClipboardMode) -> bool {
        if self.selected.is_empty() {
            return false;
        }
        self.clipboard = Some(Clipboard {
            paths: self.selected.iter().cloned().collect(),
            mode,
        });
        self.status = Some(match mode {
            ClipboardMode::Copy => "Selection ready to copy".into(),
            ClipboardMode::Move => "Selection ready to move".into(),
        });
        true
    }

    pub fn paste(&self) -> Option<PlannedOperation> {
        self.paste_into(self.directory()?.to_path_buf())
    }

    /// Paste the clipboard into `destination_dir` rather than the open folder — a right-click on
    /// a folder row.
    pub fn paste_into(&self, destination_dir: PathBuf) -> Option<PlannedOperation> {
        let clipboard = self.clipboard.as_ref()?;
        let request = match clipboard.mode {
            ClipboardMode::Copy => OperationRequest::Copy {
                sources: clipboard.paths.clone(),
                destination_dir,
            },
            ClipboardMode::Move => OperationRequest::Move {
                sources: clipboard.paths.clone(),
                destination_dir,
            },
        };
        Some(PlannedOperation {
            request,
            options: OperationOptions {
                conflict: lege_client::files::ConflictPolicy::KeepBoth,
                ..OperationOptions::default()
            },
        })
    }

    pub fn rename(&self, new_name: String) -> Result<PlannedOperation, String> {
        if !valid_file_name(&new_name) {
            return Err("Use one filename, without /, . or ...".into());
        }
        let path = self
            .single_selected()
            .cloned()
            .ok_or_else(|| "Select exactly one item to rename.".to_owned())?;
        Ok(PlannedOperation {
            request: OperationRequest::Rename { path, new_name },
            options: OperationOptions::default(),
        })
    }

    pub fn trash(&self) -> Option<PlannedOperation> {
        (!self.selected.is_empty()).then(|| PlannedOperation {
            request: OperationRequest::Trash {
                paths: self.selected.iter().cloned().collect(),
            },
            options: OperationOptions::default(),
        })
    }

    pub fn operation_pending(&mut self, id: String, summary: &str) {
        self.active_operation_id = Some(id);
        self.operation_summary = Some(summary.to_owned());
        self.status = Some(format!("Starting {summary}…"));
        self.error = None;
    }

    pub fn apply_operation(&mut self, record: OperationRecord, can_undo: bool) {
        let summary = record.kind.summary();
        let terminal = record.state.terminal();
        self.status = Some(if terminal {
            match record.state {
                lege_client::files::OperationState::Committed => format!("Finished: {summary}"),
                _ => format!(
                    "{}: {}",
                    record.error.as_deref().unwrap_or("Operation stopped"),
                    summary
                ),
            }
        } else {
            format!("{summary} — {}%", record.progress_percent())
        });
        if terminal && can_undo {
            self.undo_id = Some(record.id.clone());
        }
        if terminal {
            self.active_operation_id = None;
        }
        if terminal && matches!(record.state, lege_client::files::OperationState::Committed) {
            self.clipboard = None;
        }
        self.operation_summary = Some(summary);
        self.operation = Some(record);
    }

    pub fn operation_failed(&mut self, error: String) {
        self.error = Some(error.clone());
        self.status = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_client::files::{BrowseEntry, EntryKind, PreviewKind, PreviewMetadata};

    fn volume() -> VolumeInfo {
        VolumeInfo {
            mount_point: "/".into(),
            fs_type: "btrfs".into(),
            total_bytes: 100,
            free_bytes: 50,
            available_bytes: 45,
            reserved_bytes: 5,
            read_only: false,
        }
    }

    fn snapshot(path: &str, names: &[&str]) -> BrowseSnapshot {
        BrowseSnapshot {
            listing: DirectoryListing {
                directory: path.into(),
                parent: std::path::Path::new(path).parent().map(ToOwned::to_owned),
                entries: names
                    .iter()
                    .map(|name| BrowseEntry {
                        path: PathBuf::from(path).join(name),
                        name: (*name).into(),
                        kind: EntryKind::File,
                        is_symlink: false,
                        hidden: false,
                        size_bytes: 1,
                        mtime_ms: 1,
                    })
                    .collect(),
                truncated: false,
                limit: 500,
            },
            volume: volume(),
        }
    }

    fn trashed(name: &str, original: &str, deleted: &str) -> TrashEntry {
        TrashEntry {
            name: name.to_owned(),
            trashed_path: PathBuf::from("/home/u/.local/share/Trash/files").join(name),
            info_path: PathBuf::from("/home/u/.local/share/Trash/info")
                .join(format!("{name}.trashinfo")),
            original_path: PathBuf::from(original),
            deletion_date: deleted.to_owned(),
            trash_root: "/home/u/.local/share/Trash".into(),
            is_directory: false,
            size_bytes: 10,
        }
    }

    fn model_in_the_trash(entries: Vec<TrashEntry>) -> BrowserModel {
        let mut model = BrowserModel::new(None, false);
        let first = model.begin(PathBuf::from("/one"), Navigation::Reset);
        model.apply_browse(first.id, &first.path, Ok(snapshot("/one", &["kept"])));
        model.open_trash();
        model.apply_trash(Ok(entries));
        model
    }

    #[test]
    fn trash_is_a_place_of_its_own_and_browsing_a_folder_leaves_it() {
        let mut model = model_in_the_trash(vec![trashed(
            "report.pdf",
            "/home/u/work/report.pdf",
            "2026-08-30T18:04:11",
        )]);
        assert!(model.in_trash());
        assert_eq!(model.trash_entries().len(), 1);
        assert_eq!(model.status.as_deref(), Some("1 item in the trash."));
        assert_eq!(model.trash_entries()[0].original_location(), "/home/u/work");

        let request = model.navigate(PathBuf::from("/two"));
        assert!(!model.in_trash(), "a folder and the trash are two places");
        model.apply_browse(request.id, &request.path, Ok(snapshot("/two", &[])));
        assert_eq!(model.directory(), Some(std::path::Path::new("/two")));
    }

    #[test]
    fn trash_selection_uses_the_trash_rows_not_the_last_folders() {
        let mut model = model_in_the_trash(vec![
            trashed("a.txt", "/home/u/a.txt", "2026-08-30T18:04:11"),
            trashed("b.txt", "/home/u/b.txt", "2026-08-29T10:00:00"),
        ]);
        model.select_all();
        assert_eq!(model.selected_trashed().len(), 2);
        assert!(
            !model.selected.contains(&PathBuf::from("/one/kept")),
            "the folder underneath must not be selected by the trash's Select all"
        );
        model.select(1, SelectionModifiers::default());
        assert_eq!(
            model.selected_trashed(),
            vec![PathBuf::from("/home/u/.local/share/Trash/files/b.txt")]
        );
    }

    #[test]
    fn a_permanent_delete_needs_a_second_press_and_anything_else_cancels_it() {
        let mut model = model_in_the_trash(vec![trashed(
            "a.txt",
            "/home/u/a.txt",
            "2026-08-30T18:04:11",
        )]);
        model.select_all();

        assert_eq!(
            model.request_trash_action(ActionId::DeletePermanently),
            Ok(None),
            "the first press must only ask"
        );
        assert!(
            model
                .status
                .as_deref()
                .unwrap()
                .contains("cannot be undone")
        );
        // Walking away from the question answers it: the next press asks again.
        model.cancel_confirmation();
        assert_eq!(
            model.request_trash_action(ActionId::DeletePermanently),
            Ok(None)
        );
        assert_eq!(
            model.request_trash_action(ActionId::DeletePermanently),
            Ok(Some(TrashIntent::DeletePermanently(vec![PathBuf::from(
                "/home/u/.local/share/Trash/files/a.txt"
            )])))
        );
        assert_eq!(model.confirm, None, "the confirmation is spent");
    }

    #[test]
    fn restoring_is_reversible_so_it_never_asks_and_emptying_always_does() {
        let mut model = model_in_the_trash(vec![trashed(
            "a.txt",
            "/home/u/a.txt",
            "2026-08-30T18:04:11",
        )]);
        model.select_all();
        assert_eq!(
            model.request_trash_action(ActionId::Restore),
            Ok(Some(TrashIntent::Restore(vec![PathBuf::from(
                "/home/u/.local/share/Trash/files/a.txt"
            )])))
        );
        assert_eq!(model.request_trash_action(ActionId::EmptyTrash), Ok(None));
        assert_eq!(
            model.request_trash_action(ActionId::EmptyTrash),
            Ok(Some(TrashIntent::Empty))
        );
    }

    #[test]
    fn trash_actions_refuse_plainly_when_there_is_nothing_to_act_on() {
        let mut model = model_in_the_trash(Vec::new());
        assert_eq!(model.status.as_deref(), Some("The trash is empty."));
        assert!(model.request_trash_action(ActionId::Restore).is_err());
        assert!(model.request_trash_action(ActionId::EmptyTrash).is_err());
        assert_eq!(model.confirm, None, "a refusal must not arm anything");
    }

    #[test]
    fn a_relisting_drops_a_selection_whose_item_is_gone() {
        let mut model = model_in_the_trash(vec![
            trashed("a.txt", "/home/u/a.txt", "2026-08-30T18:04:11"),
            trashed("b.txt", "/home/u/b.txt", "2026-08-29T10:00:00"),
        ]);
        model.select_all();
        // Someone else restored a.txt; the change signal brought a fresh listing.
        model.apply_trash(Ok(vec![trashed(
            "b.txt",
            "/home/u/b.txt",
            "2026-08-29T10:00:00",
        )]));
        assert_eq!(
            model.selected_trashed(),
            vec![PathBuf::from("/home/u/.local/share/Trash/files/b.txt")]
        );
    }

    #[test]
    fn a_trash_listing_that_arrives_after_leaving_is_ignored() {
        let mut model = model_in_the_trash(Vec::new());
        let request = model.navigate(PathBuf::from("/two"));
        model.apply_browse(request.id, &request.path, Ok(snapshot("/two", &[])));
        model.apply_trash(Ok(vec![trashed(
            "late.txt",
            "/home/u/late.txt",
            "2026-08-30T18:04:11",
        )]));
        assert!(!model.in_trash());
        assert!(model.trash_entries().is_empty());
    }

    #[test]
    fn clipboard_a_copy_of_many_files_is_one_operation_over_the_whole_set() {
        let mut model = BrowserModel::new(None, false);
        let request = model.begin(PathBuf::from("/one"), Navigation::Reset);
        model.apply_browse(
            request.id,
            &request.path,
            Ok(snapshot("/one", &["a.txt", "b.txt", "c.txt"])),
        );
        model.select_all();
        assert!(model.set_clipboard(ClipboardMode::Copy));

        let second = model.navigate(PathBuf::from("/two"));
        model.apply_browse(second.id, &second.path, Ok(snapshot("/two", &[])));
        let planned = model.paste().expect("a paste into the open folder");
        match planned.request {
            OperationRequest::Copy {
                sources,
                destination_dir,
            } => {
                // One request naming all three, never three requests: one progress bar, one
                // error, one undo.
                assert_eq!(sources.len(), 3);
                assert_eq!(destination_dir, PathBuf::from("/two"));
            }
            other => panic!("a copy clipboard must paste as a copy, got {other:?}"),
        }
    }

    #[test]
    fn clipboard_a_cut_pastes_as_a_move_so_the_source_is_not_duplicated() {
        let mut model = BrowserModel::new(None, false);
        let request = model.begin(PathBuf::from("/one"), Navigation::Reset);
        model.apply_browse(request.id, &request.path, Ok(snapshot("/one", &["a.txt"])));
        model.select_all();
        assert!(model.set_clipboard(ClipboardMode::Move));

        let second = model.navigate(PathBuf::from("/two"));
        model.apply_browse(second.id, &second.path, Ok(snapshot("/two", &[])));
        assert!(matches!(
            model.paste().unwrap().request,
            OperationRequest::Move { .. }
        ));
    }

    #[test]
    fn clipboard_interchange_is_the_uri_list_every_other_application_reads() {
        let mut model = BrowserModel::new(None, false);
        let request = model.begin(PathBuf::from("/one"), Navigation::Reset);
        model.apply_browse(
            request.id,
            &request.path,
            Ok(snapshot("/one", &["a.txt", "b.txt"])),
        );
        model.select_all();
        assert_eq!(
            model.selection_as_uri_list(),
            "file:///one/a.txt\r\nfile:///one/b.txt\r\n"
        );

        // And what arrives from elsewhere becomes a clipboard here — a copy, because a bare
        // uri-list says nothing about a cut and guessing one would move somebody's original.
        assert!(model.set_clipboard_from_uri_list(
            "file:///elsewhere/one.txt\r\nfile:///elsewhere/two.txt\r\n"
        ));
        let clipboard = model.clipboard.as_ref().unwrap();
        assert_eq!(clipboard.mode, ClipboardMode::Copy);
        assert_eq!(
            clipboard.paths,
            vec![
                PathBuf::from("/elsewhere/one.txt"),
                PathBuf::from("/elsewhere/two.txt")
            ]
        );
        // Nothing pasteable in an empty payload, and no clipboard is silently discarded.
        assert!(!model.set_clipboard_from_uri_list(""));
        assert_eq!(model.clipboard.as_ref().unwrap().paths.len(), 2);
    }

    #[test]
    fn browser_ignores_a_stale_directory_response() {
        let mut model = BrowserModel::new(None, false);
        let first = model.begin(PathBuf::from("/first"), Navigation::Reset);
        let second = model.navigate(PathBuf::from("/second"));
        assert!(!model.apply_browse(first.id, &first.path, Ok(snapshot("/first", &["old"]))));
        assert!(model.listing.is_none());
        assert!(model.apply_browse(second.id, &second.path, Ok(snapshot("/second", &["new"]))));
        assert_eq!(model.directory(), Some(std::path::Path::new("/second")));
    }

    #[test]
    fn browser_history_changes_only_after_a_successful_navigation() {
        let mut model = BrowserModel::new(None, false);
        let first = model.begin(PathBuf::from("/one"), Navigation::Reset);
        model.apply_browse(first.id, &first.path, Ok(snapshot("/one", &[])));
        let failed = model.navigate(PathBuf::from("/missing"));
        model.apply_browse(failed.id, &failed.path, Err("missing".into()));
        assert!(!model.can_back());
        let second = model.navigate(PathBuf::from("/two"));
        model.apply_browse(second.id, &second.path, Ok(snapshot("/two", &[])));
        assert!(model.can_back());
        let back = model.back().unwrap();
        model.apply_browse(back.id, &back.path, Ok(snapshot("/one", &[])));
        assert_eq!(model.directory(), Some(std::path::Path::new("/one")));
        assert!(model.can_forward());
    }

    #[test]
    fn browser_selection_builds_copy_move_rename_and_trash_operations() {
        let mut model = BrowserModel::new(None, false);
        let request = model.begin(PathBuf::from("/work"), Navigation::Reset);
        model.apply_browse(
            request.id,
            &request.path,
            Ok(snapshot("/work", &["a", "b"])),
        );
        model.select(0, SelectionModifiers::default());
        assert!(model.set_clipboard(ClipboardMode::Copy));
        assert!(matches!(
            model.paste().unwrap().request,
            OperationRequest::Copy { .. }
        ));
        assert!(matches!(
            model.rename("renamed".into()).unwrap().request,
            OperationRequest::Rename { .. }
        ));
        model.select(
            1,
            SelectionModifiers {
                control: true,
                shift: false,
            },
        );
        assert!(
            matches!(model.trash().unwrap().request, OperationRequest::Trash { paths } if paths.len() == 2)
        );
        assert!(model.rename("../escape".into()).is_err());
    }

    #[test]
    fn interaction_shift_and_control_selection_have_pointer_semantics() {
        let mut model = BrowserModel::new(None, false);
        let request = model.begin(PathBuf::from("/work"), Navigation::Reset);
        model.apply_browse(
            request.id,
            &request.path,
            Ok(snapshot("/work", &["a", "b", "c"])),
        );
        model.select(0, SelectionModifiers::default());
        model.select(
            2,
            SelectionModifiers {
                shift: true,
                control: false,
            },
        );
        assert_eq!(model.selected.len(), 3);
        model.select(
            1,
            SelectionModifiers {
                control: true,
                shift: false,
            },
        );
        assert_eq!(model.selected.len(), 2);
    }

    #[test]
    fn preview_ignores_a_late_result_for_the_previous_selection() {
        let mut model = BrowserModel::new(None, false);
        let browse = model.begin(PathBuf::from("/work"), Navigation::Reset);
        model.apply_browse(
            browse.id,
            &browse.path,
            Ok(snapshot("/work", &["first.jxl", "second.heic"])),
        );
        model.select(0, SelectionModifiers::default());
        let first = model.begin_preview().unwrap();
        model.select(1, SelectionModifiers::default());
        let second = model.begin_preview().unwrap();
        assert!(!model.apply_preview(first.id, &first.path, Ok(None)));
        let preview = PreviewSnapshot {
            metadata: PreviewMetadata {
                renderer: "test".into(),
                kind: PreviewKind::Heic,
                width: 1,
                height: 1,
            },
            bitmap: Bitmap {
                width: 1,
                height: 1,
                pixels: vec![0xffff_ffff],
            },
        };
        assert!(model.apply_preview(second.id, &second.path, Ok(Some(preview))));
        assert_eq!(
            model.preview.as_ref().unwrap().metadata.kind,
            PreviewKind::Heic
        );
    }

    fn seeded_model(names: &[&str]) -> BrowserModel {
        let mut model = BrowserModel::new(None, false);
        let request = model.navigate("/home/u/docs".into());
        model.apply_browse(
            request.id,
            std::path::Path::new("/home/u/docs"),
            Ok(snapshot("/home/u/docs", names)),
        );
        model
    }

    /// The app re-sorts with the same shared contract the service serves by, selection is a
    /// path set that survives re-ordering, and keyboard focus follows its item to its new row.
    #[test]
    fn sort_reorders_by_the_shared_contract_and_keeps_selection_and_focus() {
        let mut model = seeded_model(&["file10.txt", "file2.txt", "alpha.txt"]);
        // Service default order: natural name.
        let names: Vec<&str> = model
            .listing
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["alpha.txt", "file2.txt", "file10.txt"]);

        model.select(1, SelectionModifiers::default());
        model.select(
            2,
            SelectionModifiers {
                control: true,
                shift: false,
            },
        );
        let focused = model.focused_path().cloned().unwrap();
        assert_eq!(focused, PathBuf::from("/home/u/docs/file10.txt"));

        model.set_sort(lege_types::file_sort::SortDescriptor {
            field: lege_types::file_sort::SortField::NaturalName,
            ascending: false,
            folders_first: true,
        });
        let names: Vec<&str> = model
            .listing
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["file10.txt", "file2.txt", "alpha.txt"]);
        assert_eq!(
            model.selected.len(),
            2,
            "the selected path set is untouched"
        );
        assert_eq!(
            model.focused_path().cloned().unwrap(),
            focused,
            "focus followed its item to the new row"
        );
    }

    /// Switching between list and thumbnails changes only how rows are drawn: the same
    /// selection, the same focus, back and forth without loss.
    #[test]
    fn view_mode_switch_preserves_selection_and_focus() {
        let mut model = seeded_model(&["a.png", "b.png", "c.png"]);
        model.select(1, SelectionModifiers::default());
        let focused = model.focused_path().cloned();
        let selected = model.selected.clone();

        model.set_view(lege_types::file_sort::ViewMode::Thumbnails);
        assert_eq!(model.view, lege_types::file_sort::ViewMode::Thumbnails);
        assert_eq!(model.selected, selected);
        assert_eq!(model.focused_path().cloned(), focused);

        model.set_view(lege_types::file_sort::ViewMode::List);
        assert_eq!(model.selected, selected);
        assert_eq!(model.focused_path().cloned(), focused);
    }

    /// Thumbnails load boundedly: only the visible window is requested, each batch is capped,
    /// nothing is requested twice, and a response from before the listing changed is dropped.
    #[test]
    fn thumbnail_requests_are_bounded_visible_only_and_stale_responses_are_dropped() {
        let names: Vec<String> = (0..40)
            .map(|index| format!("photo{index:02}.png"))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut model = seeded_model(&names);
        assert!(
            model.begin_thumbnails(0..10).is_empty(),
            "list mode requests nothing"
        );

        model.set_view(lege_types::file_sort::ViewMode::Thumbnails);
        let requests = model.begin_thumbnails(0..40);
        assert_eq!(
            requests.len(),
            BrowserModel::THUMBNAIL_BATCH,
            "one bounded batch, however long the directory"
        );
        let visible_only = model.begin_thumbnails(0..4);
        assert!(
            visible_only.len() <= 4,
            "only the visible window is asked for"
        );

        let first = requests[0].clone();
        assert!(model.apply_thumbnail(first.generation, &first.path, Some(Bitmap::new(1, 1))));
        assert!(
            !model
                .begin_thumbnails(0..40)
                .iter()
                .any(|request| request.path == first.path),
            "a loaded thumbnail is not requested again"
        );

        // The listing changes: outstanding decodes are from a dead generation.
        let request = model.navigate("/home/u/other".into());
        model.apply_browse(
            request.id,
            std::path::Path::new("/home/u/other"),
            Ok(snapshot("/home/u/other", &["x.png"])),
        );
        assert!(
            !model.apply_thumbnail(first.generation, &first.path, Some(Bitmap::new(1, 1))),
            "a stale response is dropped, not painted into the wrong place"
        );
        assert!(model.thumbnails.is_empty());
    }
}
