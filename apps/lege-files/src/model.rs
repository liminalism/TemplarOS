//! Pure file-manager state: stale-safe browsing, history, selection and operation intent.

use crate::actions::ActionId;
use lege_client::files::{
    BrowserSources, DirectoryListing, FileInfo, OperationOptions, OperationRecord,
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
}

impl BrowserModel {
    pub fn new(requested_initial: Option<PathBuf>, show_hidden: bool) -> Self {
        Self {
            sources: BrowserSources::default(),
            listing: None,
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
                self.listing = Some(snapshot.listing);
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
        let clipboard = self.clipboard.as_ref()?;
        let destination_dir = self.directory()?.to_path_buf();
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
}
