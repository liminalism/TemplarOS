//! Application wiring: pure model + asynchronous service backend + Pixelkit window.

use anyhow::{Context, Result};
use pixelkit_raster::WindowBuffer;
use pixelkit_shell::{
    KeyInput, Modifiers, MouseButton, PixelApp, Scale, Waker, WindowConfig, run_app,
};
use std::path::{Path, PathBuf};

use crate::{
    actions::{ActionId, Command},
    backend::{Backend, Update},
    model::{
        BrowseRequest, BrowserModel, ClipboardMode, PlannedOperation, SelectionModifiers,
        TrashIntent,
    },
    surface::FileSurface,
};

#[derive(Debug)]
struct FilesApp {
    model: BrowserModel,
    backend: Backend,
    surface: FileSurface,
}

impl FilesApp {
    fn new(path: Option<PathBuf>, show_hidden: bool) -> Result<Self> {
        Ok(Self {
            model: BrowserModel::new(path, show_hidden),
            backend: Backend::new()?,
            surface: FileSurface::default(),
        })
    }

    fn drain_updates(&mut self) {
        while let Some(update) = self.backend.try_update() {
            match update {
                Update::Roots(result) => match result {
                    Ok(sources) => {
                        if let Some(request) = self.model.set_sources(sources) {
                            self.backend.browse(request);
                        }
                    }
                    Err(error) => self.model.operation_failed(error),
                },
                Update::Browse { request, result } => {
                    self.model.apply_browse(request.id, &request.path, result);
                }
                Update::Inspect { request, result } => {
                    self.model.apply_inspect(request.id, &request.path, result);
                }
                Update::Preview { request, result } => {
                    self.model.apply_preview(request.id, &request.path, result);
                }
                Update::Thumbnail { request, bitmap } => {
                    self.model
                        .apply_thumbnail(request.generation, &request.path, bitmap);
                }
                Update::Notice(text) => {
                    self.model.error = None;
                    self.model.status = Some(text);
                }
                Update::CopiedPath { path, result } => match result {
                    Ok(copied) => {
                        self.model.error = None;
                        self.model.status = Some(format!("Copied path: {copied}"));
                    }
                    Err(error) => self
                        .model
                        .operation_failed(format!("Could not copy {}: {error}", path.display())),
                },
                Update::Opened { path, result } => match result {
                    Ok(opened) => {
                        self.model.error = None;
                        self.model.status = Some(format!(
                            "Opened {} with {} · instance {}",
                            path.display(),
                            opened.app_id,
                            opened.instance
                        ));
                    }
                    Err(error) => self
                        .model
                        .operation_failed(format!("Could not open {}: {error}", path.display())),
                },
                Update::OperationStarted { id, summary } => {
                    self.model.operation_pending(id, &summary);
                }
                Update::OperationProgress { record, can_undo } => {
                    let terminal = record.state.terminal();
                    self.model.apply_operation(record, can_undo);
                    if terminal && let Some(request) = self.model.refresh() {
                        self.backend.browse(request);
                    }
                }
                Update::OperationFailed { summary, error } => {
                    self.model.operation_failed(format!("{summary}: {error}"));
                }
                Update::Trash(result) => self.model.apply_trash(result),
                // Something changed the trash — this window, another one, Home, or the CLI. Only
                // re-list when the trash is what is on screen; otherwise there is nothing to
                // refresh and a listing would arrive for a place nobody is looking at.
                Update::TrashChanged => {
                    if self.model.in_trash() {
                        self.backend.trash_list();
                    }
                }
                Update::Cancelled(result) => match result {
                    Ok(()) => self.model.status = Some("Cancellation requested…".into()),
                    Err(error) => self.model.operation_failed(error),
                },
            }
        }
    }

    fn inspect_selection(&mut self) {
        if let Some(request) = self.model.begin_inspect() {
            self.backend.inspect(request);
        }
    }

    fn preview_selection(&mut self) {
        if let Some(request) = self.model.begin_preview() {
            self.backend.preview(request);
        }
    }

    fn browse(&self, request: Option<BrowseRequest>) {
        if let Some(request) = request {
            self.backend.browse(request);
        }
    }

    fn start_operation(&mut self, operation: Option<PlannedOperation>) {
        if let Some(operation) = operation {
            let summary = operation.request.summary();
            self.model.status = Some(format!("Starting {summary}…"));
            self.backend
                .start_operation(operation.request, operation.options);
        }
    }

    fn activate_selection(&mut self) {
        let Some(path) = self
            .model
            .focused_path()
            .cloned()
            .or_else(|| self.model.single_selected().cloned())
        else {
            return;
        };
        let is_dir = self
            .model
            .listing
            .as_ref()
            .and_then(|listing| listing.entries.iter().find(|entry| entry.path == path))
            .is_some_and(lege_client::files::BrowseEntry::is_dir);
        if is_dir {
            let request = self.model.navigate(path);
            self.backend.browse(request);
        } else {
            self.model.status = Some(format!("Opening {}…", path.display()));
            self.backend.open(path);
        }
    }

    /// One trash action, through the model's confirmation rule. Nothing else may reach
    /// `Backend::trash_action`, so no route can be the one that skips the question.
    fn handle_trash_action(&mut self, action: ActionId) {
        match self.model.request_trash_action(action) {
            Ok(Some(intent)) => {
                self.model.status = Some(match &intent {
                    TrashIntent::Restore(paths) => {
                        format!("Restoring {} item(s)…", paths.len())
                    }
                    TrashIntent::DeletePermanently(paths) => {
                        format!("Deleting {} item(s) permanently…", paths.len())
                    }
                    TrashIntent::Empty => "Emptying the trash…".to_owned(),
                });
                self.backend.trash_action(intent);
            }
            // The prompt is already in `status`; there is nothing else to say.
            Ok(None) => {}
            Err(message) => self.model.status = Some(message),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_action(&mut self, action: ActionId, modifiers: SelectionModifiers) {
        // Any action other than repeating the destructive one answers its question with "no".
        if self.model.confirm != Some(action) {
            self.model.cancel_confirmation();
        }
        match action {
            ActionId::Back => {
                let request = self.model.back();
                self.browse(request);
            }
            ActionId::Forward => {
                let request = self.model.forward();
                self.browse(request);
            }
            ActionId::Up => {
                let request = self.model.up();
                self.browse(request);
            }
            ActionId::Refresh => {
                let request = self.model.refresh();
                self.browse(request);
            }
            ActionId::CycleSort => {
                let sort = self.model.sort;
                let field = match sort.field {
                    lege_types::file_sort::SortField::NaturalName => {
                        lege_types::file_sort::SortField::Kind
                    }
                    lege_types::file_sort::SortField::Kind => {
                        lege_types::file_sort::SortField::Size
                    }
                    lege_types::file_sort::SortField::Size => {
                        lege_types::file_sort::SortField::Modified
                    }
                    lege_types::file_sort::SortField::Modified
                    | lege_types::file_sort::SortField::Manual => {
                        lege_types::file_sort::SortField::NaturalName
                    }
                };
                self.model
                    .set_sort(lege_types::file_sort::SortDescriptor { field, ..sort });
            }
            ActionId::ToggleSortDirection => {
                let sort = self.model.sort;
                self.model.set_sort(lege_types::file_sort::SortDescriptor {
                    ascending: !sort.ascending,
                    ..sort
                });
            }
            ActionId::ToggleView => {
                self.model.set_view(match self.model.view {
                    lege_types::file_sort::ViewMode::List => {
                        lege_types::file_sort::ViewMode::Thumbnails
                    }
                    lege_types::file_sort::ViewMode::Thumbnails => {
                        lege_types::file_sort::ViewMode::List
                    }
                });
            }
            ActionId::ToggleHidden => {
                let request = self.model.toggle_hidden();
                self.browse(request);
            }
            ActionId::Location => self.surface.begin_location(
                self.model
                    .pending_path()
                    .or_else(|| self.model.directory())
                    .map_or_else(String::new, |path| path.display().to_string()),
            ),
            ActionId::Open => self.activate_selection(),
            ActionId::Copy => {
                self.model.set_clipboard(ClipboardMode::Copy);
            }
            ActionId::CopyPath => {
                if let Some(path) = self.model.single_selected().cloned() {
                    self.model.status = Some(format!("Copying {}…", path.display()));
                    self.backend.copy_path(path);
                } else {
                    self.model.status = Some("Select exactly one item to copy its path.".into());
                }
            }
            ActionId::Cut => {
                self.model.set_clipboard(ClipboardMode::Move);
            }
            ActionId::Paste => {
                let operation = self.model.paste();
                self.start_operation(operation);
            }
            ActionId::Rename => {
                if let Some(path) = self.model.single_selected() {
                    let name = path
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
                    self.surface.begin_rename(name);
                } else {
                    self.model.status = Some("Select exactly one item to rename.".into());
                }
            }
            ActionId::Trash => {
                let operation = self.model.trash();
                self.start_operation(operation);
            }
            ActionId::OpenTrash => {
                self.model.open_trash();
                self.backend.trash_list();
            }
            ActionId::Restore | ActionId::DeletePermanently | ActionId::EmptyTrash => {
                if self.model.in_trash() {
                    self.handle_trash_action(action);
                } else {
                    self.model.status =
                        Some(format!("{} only applies in the trash.", action.label()));
                }
            }
            ActionId::Undo => {
                if let Some(id) = self.model.undo_id.take() {
                    self.backend.undo(id);
                } else {
                    self.model.status = Some("Nothing available to undo.".into());
                }
            }
            ActionId::Properties => {
                if self.model.single_selected().is_some() {
                    self.inspect_selection();
                } else {
                    self.model.status = Some("Select exactly one item for properties.".into());
                }
            }
            ActionId::CancelOperation => {
                if let Some(id) = self.model.active_operation_id.clone() {
                    self.backend.cancel(id);
                }
            }
            ActionId::SelectPrevious => {
                self.model
                    .move_focus(self.model.focus.saturating_sub(1), modifiers.shift);
                self.preview_selection();
            }
            ActionId::SelectNext => {
                self.model
                    .move_focus(self.model.focus.saturating_add(1), modifiers.shift);
                self.preview_selection();
            }
            ActionId::SelectFirst => {
                self.model.move_focus(0, modifiers.shift);
                self.preview_selection();
            }
            ActionId::SelectLast => {
                self.model.move_focus(usize::MAX, modifiers.shift);
                self.preview_selection();
            }
            ActionId::SelectAll => {
                self.model.select_all();
                self.preview_selection();
            }
            ActionId::ClearSelection => self.model.clear_selection(),
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Action(action, modifiers) => self.handle_action(action, modifiers),
            Command::Navigate(path) => {
                let request = self.model.navigate(path);
                self.backend.browse(request);
            }
            Command::ThumbnailsVisible(range) => {
                for request in self.model.begin_thumbnails(range) {
                    self.backend.thumbnail(request);
                }
            }
            Command::Select {
                index,
                modifiers,
                activate_if_selected,
            } => {
                let activate = self.model.select(index, modifiers);
                self.preview_selection();
                if activate && activate_if_selected {
                    self.activate_selection();
                }
            }
            Command::ConfirmRename(new_name) => match self.model.rename(new_name) {
                Ok(operation) => {
                    self.surface.dismiss_dialog();
                    self.start_operation(Some(operation));
                }
                Err(error) => self.surface.set_dialog_error(error),
            },
            Command::ConfirmLocation(location) => {
                let path = PathBuf::from(location);
                if path.is_absolute() {
                    self.surface.dismiss_dialog();
                    let request = self.model.navigate(path);
                    self.backend.browse(request);
                } else {
                    self.surface
                        .set_dialog_error("Enter an absolute path beginning with /.".into());
                }
            }
            Command::DismissDialog => self.surface.dismiss_dialog(),
            Command::OpenWindow(path) => self.backend.open_window(path),
            Command::Bookmark { path, enabled } => self.backend.bookmark(path, enabled),
            Command::PasteInto(path) => {
                let operation = self.model.paste_into(path);
                self.start_operation(operation);
            }
            Command::AddToHome(path) => self.backend.add_to_home(path),
            // Opened by the surface itself; it never reaches here.
            Command::ContextMenu { .. } => {}
        }
    }
}

impl PixelApp for FilesApp {
    fn render(&mut self, buffer: &mut WindowBuffer, scale: Scale) {
        self.drain_updates();
        let commands = self.surface.draw(buffer, scale, &self.model);
        for command in commands {
            self.handle_command(command);
        }
    }

    fn attach(&mut self, waker: Waker) {
        self.backend.attach(waker);
        self.backend.roots();
        // Started with the window, not with the trash view: the signal is what tells this window
        // the trash changed, and a subscription that only exists while the trash is open would
        // miss exactly the change that happened while the user was elsewhere.
        self.backend.watch_trash();
    }

    fn tick(&mut self) {
        self.drain_updates();
    }

    fn on_key(&mut self, key: &KeyInput) {
        self.surface.input.key(key.clone());
    }

    fn on_modifiers(&mut self, modifiers: Modifiers) {
        self.surface.input.modifiers_changed(modifiers);
    }

    fn on_cursor(&mut self, x: f32, y: f32) {
        self.surface.input.cursor_moved(x, y);
    }

    fn on_cursor_left(&mut self) {
        self.surface.input.cursor_left();
    }

    fn on_mouse(&mut self, button: MouseButton, pressed: bool) {
        self.surface.input.mouse(button, pressed);
    }

    fn on_scroll(&mut self, dx: f32, dy: f32) {
        self.surface.input.scrolled(dx, dy);
    }
}

pub fn run_graphical(path: Option<PathBuf>, show_hidden: bool) -> Result<()> {
    let app = FilesApp::new(path, show_hidden)?;
    run_app(
        app,
        WindowConfig::new("Files", 1180.0, 760.0).min_size(760.0, 480.0),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

pub fn run_print(path: Option<&Path>, show_hidden: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("creating print runtime")?;
    let text = runtime.block_on(crate::backend::print_snapshot(path, show_hidden))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_graphical_configuration_is_a_resizable_manager_window() {
        let config = WindowConfig::new("Files", 1180.0, 760.0).min_size(760.0, 480.0);
        assert!(config.resizable);
        assert_eq!(config.min_width, Some(760.0));
        assert_eq!(config.min_height, Some(480.0));
    }
}
