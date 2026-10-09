//! Native portal windows sharing the process's persistent main-thread PixelKit host.
//!
//! This module is deliberately thin: *what* the file dialog is showing, and what a click means,
//! is [`crate::browser::Browser`], which has no display in it and is tested. What is left here
//! is the drawing — a virtualised list so a directory of any size scrolls, a hidden-file
//! checkbox, a filter menu, and a name field when saving.

use crate::{
    browser::{Activation, Browser, FileFilter, FilterPattern},
    file_chooser::{ChooserKind, ChooserRequest},
};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, Modifiers, MouseButton, Scale};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{Dropdown, Focus, ScrollState, TextFieldState, Theme, Ui};
use pixelkit_windowing::{PixelApp, WindowRequest};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Answer {
    Choice(String),
    Files(FileSelection),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileSelection {
    #[serde(with = "crate::wire::paths")]
    pub paths: Vec<PathBuf>,
    pub current_filter: Option<FileFilter>,
}

/// Initial request data, before listing directories or constructing widget state.
/// X11-parented requests can move to an owned X11 main-thread host without losing
/// the parent handle, activation token, filter or Unix filename bytes.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Spec {
    Files(ChooserRequest),
    Choice {
        title: String,
        message: String,
        choices: Vec<String>,
        parent: crate::parent::Parent,
        activation_token: Option<String>,
    },
}

impl Spec {
    pub(crate) fn parent(&self) -> &crate::parent::Parent {
        match self {
            Self::Files(request) => &request.parent,
            Self::Choice { parent, .. } => parent,
        }
    }

    pub(crate) fn into_model(self) -> Model {
        match self {
            Self::Files(request) => files(request),
            Self::Choice {
                title,
                message,
                choices,
                parent,
                activation_token,
            } => {
                let mut model = choice(title, message, choices);
                model.set_parent(parent);
                model.set_activation_token(activation_token);
                model
            }
        }
    }
}

#[derive(Debug)]
enum Mode {
    Choice {
        message: String,
        choices: Vec<String>,
        selected: usize,
    },
    Files(FileMode),
}

/// The file dialog's own state: the model, plus the three pieces of widget state a scrolling
/// list, a filter menu and a name field need to keep between frames.
#[derive(Debug)]
struct FileMode {
    browser: Browser,
    scroll: ScrollState,
    filter_menu: Dropdown,
    name: TextFieldState,
    cursor: usize,
    replace: Option<FileSelection>,
}

#[derive(Debug)]
pub struct Model {
    title: String,
    mode: Mode,
    answer: Option<Answer>,
    exit: bool,
    error: Option<String>,
    activation_token: Option<String>,
    parent: crate::parent::Parent,
    parent_notice: Option<String>,
}

impl Model {
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn activation_token(&self) -> Option<&str> {
        self.activation_token.as_deref()
    }
    pub fn set_activation_token(&mut self, token: Option<String>) {
        self.activation_token = token;
    }
    pub fn set_parent(&mut self, parent: crate::parent::Parent) {
        self.parent = parent;
    }
    fn choose(&mut self) {
        let answer = match &mut self.mode {
            Mode::Choice {
                choices, selected, ..
            } => choices.get(*selected).cloned().map(Answer::Choice),
            Mode::Files(files) => {
                if files.replace.is_some() {
                    return;
                }
                let paths = match files.browser.choose(files.name.text()) {
                    Ok(paths) => paths,
                    Err(message) => {
                        self.error = Some(message.to_owned());
                        return;
                    }
                };
                let selection = FileSelection {
                    paths,
                    current_filter: files
                        .browser
                        .current_filter()
                        .and_then(|index| files.browser.filters().get(index))
                        .cloned()
                        .or_else(|| {
                            (!files.browser.filters().is_empty()).then(|| FileFilter {
                                name: "All files".into(),
                                patterns: vec![FilterPattern::Glob("*".into())],
                            })
                        }),
                };
                if matches!(files.browser.kind(), ChooserKind::Save { .. }) {
                    let target = &selection.paths[0];
                    match std::fs::symlink_metadata(target) {
                        Ok(_) => {
                            files.replace = Some(selection);
                            self.error = None;
                            return;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            self.error = Some(format!("Cannot inspect save target: {error}"));
                            return;
                        }
                    }
                }
                self.error = None;
                Some(Answer::Files(selection))
            }
        };
        if let Some(answer) = answer {
            self.answer = Some(answer);
            self.exit = true;
        }
    }

    fn confirming_replace(&self) -> bool {
        matches!(&self.mode, Mode::Files(files) if files.replace.is_some())
    }

    fn confirm_replace(&mut self) {
        if let Mode::Files(files) = &mut self.mode
            && let Some(selection) = files.replace.take()
        {
            self.answer = Some(Answer::Files(selection));
            self.exit = true;
        }
    }

    fn cancel(&mut self) {
        if let Mode::Files(files) = &mut self.mode
            && files.replace.take().is_some()
        {
            self.error = None;
            return;
        }
        self.exit = true;
    }
}

/// Draw the body for whichever mode this dialog is in. Returns whether the listing must be
/// re-read once `mode` is free again (the browser is borrowed for the whole of the drawing).
fn draw_mode(
    ui: &mut Ui<'_>,
    area: Rect,
    mode: &mut Mode,
    focus: &mut Focus,
    accept: &mut bool,
) -> bool {
    match mode {
        Mode::Choice {
            message,
            choices,
            selected,
        } => {
            ui.label(
                Rect::new(area.x, area.y + ui.px(48), area.w, ui.px(36)),
                message,
                ui.theme.text_dim,
                Align::Left,
            );
            let labels: Vec<_> = choices.iter().map(String::as_str).collect();
            let group = Rect::new(area.x, area.y + ui.px(92), area.w, ui.px(260));
            let focused = focus.register_at(ui.input, group);
            *accept |= focused && ui.input.keys().contains(&KeyInput::Enter);
            ui.radio_group(selected, group, &labels, focused);
            false
        }
        Mode::Files(files) => draw_files(ui, area, files, focus, accept),
    }
}

/// The file browser: a parent button and the current path, the hidden/filter controls, the
/// scrolling listing, and a name field when saving. Returns whether the listing must be re-read.
fn draw_files(
    ui: &mut Ui<'_>,
    area: Rect,
    files: &mut FileMode,
    focus: &mut Focus,
    accept: &mut bool,
) -> bool {
    if let Some(selection) = &files.replace {
        ui.label(
            Rect::new(area.x, area.y + ui.px(70), area.w, ui.px(36)),
            "This file already exists. Replace it?",
            ui.theme.text,
            Align::Left,
        );
        ui.label(
            Rect::new(area.x, area.y + ui.px(110), area.w, ui.px(64)),
            &selection.paths[0].display().to_string(),
            ui.theme.text_dim,
            Align::Left,
        );
        return false;
    }
    let mut stale = false;

    let up = Rect::new(area.x, area.y + ui.px(50), ui.px(54), ui.px(34));
    let focused = focus.register_at(ui.input, up);
    if ui.button_focused(up, "Up", focused) && files.browser.go_up() {
        stale = true;
    }
    let directory_label = files.browser.directory().display().to_string();
    ui.label(
        Rect::new(
            area.x + ui.px(64),
            area.y + ui.px(50),
            area.w - ui.px(64),
            ui.px(34),
        ),
        &directory_label,
        ui.theme.text_dim,
        Align::Left,
    );

    stale |= draw_controls(ui, area, files, focus);

    // SaveFiles asks for a directory; the application owns every filename.
    // Showing an editable name here would promise an edit the reply ignores.
    let saving = matches!(files.browser.kind(), ChooserKind::Save { .. });
    let list_bottom = area.bottom() - if saving { ui.px(104) } else { ui.px(56) };
    let list = Rect::new(
        area.x,
        area.y + ui.px(132),
        area.w,
        (list_bottom - (area.y + ui.px(132))).max(ui.px(30)),
    );
    if let Some(activated) = draw_listing(ui, list, files, focus) {
        match files.browser.activate(activated) {
            Some(Activation::Entered(_)) => {
                files.scroll = ScrollState::new();
                stale = true;
            }
            Some(Activation::Picked(path)) => {
                // A save dialog's name field follows the file you clicked, so "overwrite that
                // one" is a click rather than retyping its name.
                if saving && let Some(name) = path.file_name() {
                    files.name.set_text(name.to_string_lossy().into_owned());
                }
            }
            None => {}
        }
    }

    if saving {
        let name_area = Rect::new(area.x, area.bottom() - ui.px(96), area.w, ui.px(38));
        let focused = focus.register_at(ui.input, name_area);
        *accept |= focused && ui.input.keys().contains(&KeyInput::Enter);
        ui.text_field(&mut files.name, name_area, "File name", focused, false);
        if let Some(error) = files.name.clipboard_error() {
            ui.label(
                Rect::new(area.x, name_area.y - ui.px(24), area.w, ui.px(22)),
                error,
                ui.theme.alarm,
                Align::Left,
            );
        }
    }
    if matches!(
        files.browser.kind(),
        ChooserKind::Open {
            multiple: true,
            directory: true
        }
    ) {
        ui.label(
            Rect::new(
                area.x,
                area.bottom() - ui.px(48),
                area.w - ui.px(240),
                ui.px(38),
            ),
            &format!("{} folders selected", files.browser.selected_count()),
            ui.theme.text_dim,
            Align::Left,
        );
    }

    // Drawn last so the open menu covers the rows underneath it, not the other way round.
    if let Some(picked) = files.filter_menu.draw(ui, area) {
        files
            .browser
            .set_current_filter(if picked == 0 { None } else { Some(picked - 1) });
        files.scroll = ScrollState::new();
    }
    stale
}

/// The "Show hidden" checkbox, the filter menu and the truncation notice. Returns whether the
/// listing must be re-read (`templar-filed` excludes hidden entries at read time).
fn draw_controls(ui: &mut Ui<'_>, area: Rect, files: &mut FileMode, focus: &mut Focus) -> bool {
    let mut show_hidden = files.browser.show_hidden();
    let hidden = Rect::new(area.x, area.y + ui.px(94), ui.px(160), ui.px(28));
    let focused = focus.register_at(ui.input, hidden);
    let toggled = ui.checkbox_focused(hidden, &mut show_hidden, "Show hidden files", focused);
    if toggled {
        files.browser.set_show_hidden(show_hidden);
    }

    if !files.browser.filters().is_empty() {
        let mut labels = vec!["All files".to_owned()];
        labels.extend(
            files
                .browser
                .filters()
                .iter()
                .map(|filter| filter.name.clone()),
        );
        let borrowed: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = files.browser.current_filter().map_or(0, |index| index + 1);
        let filter_area = Rect::new(
            area.right() - ui.px(220),
            area.y + ui.px(92),
            ui.px(220),
            ui.px(32),
        );
        let focused = focus.register_at(ui.input, filter_area);
        if let Some(picked) = ui.dropdown_focused(
            &mut files.filter_menu,
            filter_area,
            &borrowed,
            selected,
            focused,
        ) {
            files
                .browser
                .set_current_filter(if picked == 0 { None } else { Some(picked - 1) });
            files.scroll = ScrollState::new();
        }
    }

    if files.browser.truncated() {
        ui.label(
            Rect::new(area.x + ui.px(176), area.y + ui.px(94), area.w, ui.px(28)),
            "This folder has more files than the listing can carry.",
            ui.theme.alarm,
            Align::Left,
        );
    }
    toggled
}

/// The listing itself, virtualised: however many entries a folder holds, only the rows on screen
/// are drawn, and the rest are a scroll away. Returns the index of an activated row.
fn draw_listing(
    ui: &mut Ui<'_>,
    area: Rect,
    files: &mut FileMode,
    focus: &mut Focus,
) -> Option<usize> {
    let visible = files.browser.visible();
    let multiple_folders = matches!(
        files.browser.kind(),
        ChooserKind::Open {
            multiple: true,
            directory: true
        }
    );
    let focused = focus.register_at(ui.input, area);
    if visible.is_empty() {
        ui.label(
            Rect::new(area.x, area.y, area.w, ui.px(30)),
            "Nothing here to choose.",
            ui.theme.text_dim,
            Align::Left,
        );
        return None;
    }

    let labels: Vec<String> = visible
        .iter()
        .map(|entry| {
            if entry.is_dir() {
                format!(
                    "{}[{}]",
                    if files.browser.is_selected(&entry.path) {
                        "✓ "
                    } else {
                        ""
                    },
                    entry.name
                )
            } else if files.browser.is_selected(&entry.path) {
                format!("✓ {}", entry.name)
            } else {
                entry.name.clone()
            }
        })
        .collect();
    let row = ui.px(34);
    let heights = vec![row; labels.len()];
    files.cursor = files.cursor.min(labels.len() - 1);
    let mut activated = None;
    let mut picked = None;
    if focused {
        ui.input.consume_keys(|keys| {
            for key in keys {
                match key {
                    KeyInput::Up => files.cursor = files.cursor.saturating_sub(1),
                    KeyInput::Down => files.cursor = (files.cursor + 1).min(labels.len() - 1),
                    KeyInput::Home => files.cursor = 0,
                    KeyInput::End => files.cursor = labels.len() - 1,
                    KeyInput::Character(' ') if multiple_folders => picked = Some(files.cursor),
                    KeyInput::Enter | KeyInput::Character(' ') => activated = Some(files.cursor),
                    _ => {}
                }
            }
        });
        files.scroll.reveal(
            files.cursor as i32 * row,
            row,
            area.h,
            labels.len() as i32 * row,
        );
    }
    ui.scroll_list(&mut files.scroll, area, &heights, |ui, index, rect| {
        let inner = Rect::new(
            rect.x,
            rect.y,
            rect.w - if multiple_folders { ui.px(100) } else { 0 },
            rect.h - ui.px(4),
        );
        if ui.button_focused(inner, &labels[index], focused && files.cursor == index) {
            files.cursor = index;
            activated = Some(index);
        }
        if multiple_folders {
            let select = Rect::new(
                rect.right() - ui.px(96),
                rect.y,
                ui.px(96),
                rect.h - ui.px(4),
            );
            let select_focused = focus.register_at(ui.input, select);
            let label = if files.browser.is_selected(&visible[index].path) {
                "Remove"
            } else {
                "Select"
            };
            if ui.button_focused(select, label, select_focused) {
                picked = Some(index);
            }
        }
    });
    let picked_path = picked
        .and_then(|index| visible.get(index))
        .map(|entry| entry.path.clone());
    if let Some(path) = picked_path {
        files.browser.pick(path);
    }
    activated
}

struct Surface {
    input: Input,
    text: TextCache,
    kernel: RasterKernel,
    focus: Focus,
    semantics: pixelkit_ui::semantics::Semantics,
    theme: Theme,
    semantics_active: bool,
}

impl Surface {
    fn new() -> Self {
        Self {
            input: Input::new(),
            text: TextCache::new(fonts()),
            kernel: RasterKernel::new(),
            focus: Focus::new(),
            semantics: Default::default(),
            theme: theme(),
            semantics_active: false,
        }
    }

    fn draw(&mut self, buffer: &mut WindowBuffer, scale: Scale, model: &mut Model) {
        let whole = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
        let was_confirming = model.confirming_replace();
        self.focus
            .handle_tab(self.input.keys(), self.input.modifiers());
        if self.focus.current().is_none() {
            self.focus.set(0);
        }
        if self.semantics_active {
            if was_confirming {
                self.semantics.begin_scoped(&model.title, 1);
            } else if matches!(&model.mode, Mode::Choice { .. }) {
                self.semantics.begin_scoped(&model.title, 0);
            } else {
                self.semantics.begin(&model.title);
            }
        }
        let ui = Ui::new(
            Painter::new(buffer),
            &mut self.text,
            &mut self.input,
            self.theme,
            scale,
            &mut self.kernel,
        );
        let mut ui = if self.semantics_active {
            ui.with_semantics(&mut self.semantics)
        } else {
            ui
        };
        ui.painter.fill_rect(whole, ui.theme.background);
        let area = whole.inset(ui.px(20));
        ui.heading(Rect::new(area.x, area.y, area.w, ui.px(44)), &model.title);
        if let Some(notice) = &model.parent_notice {
            ui.label(
                Rect::new(area.x, area.y + ui.px(38), area.w, ui.px(20)),
                notice,
                ui.theme.text_dim,
                Align::Left,
            );
        }
        // The directory listing is re-read after the match: `model.mode` is borrowed for the
        // whole of it, so navigation only records that a refresh is due.
        let mut accept_requested = false;
        // Reserve a stable line for request-local parenting diagnostics without covering controls.
        let body = Rect::new(area.x, area.y + ui.px(24), area.w, area.h - ui.px(24));
        let listing_is_stale = draw_mode(
            &mut ui,
            body,
            &mut model.mode,
            &mut self.focus,
            &mut accept_requested,
        );
        if listing_is_stale && let Mode::Files(files) = &mut model.mode {
            files.browser.refresh();
        }
        if let Some(error) = &model.error {
            ui.label(
                Rect::new(
                    area.x,
                    area.bottom() - ui.px(48),
                    area.w - ui.px(240),
                    ui.px(38),
                ),
                error,
                ui.theme.alarm,
                Align::Left,
            );
        }
        let cancel = Rect::new(
            area.right() - ui.px(220),
            area.bottom() - ui.px(44),
            ui.px(100),
            ui.px(38),
        );
        let accept = Rect::new(
            area.right() - ui.px(108),
            area.bottom() - ui.px(44),
            ui.px(108),
            ui.px(38),
        );
        let focused = self.focus.register_at(ui.input, cancel);
        let cancelled = ui.button_focused(cancel, "Cancel", focused);
        if cancelled {
            model.cancel();
        }
        let focused = self.focus.register_at(ui.input, accept);
        let accepted = ui.button_focused(
            accept,
            if was_confirming { "Replace" } else { "Choose" },
            focused,
        );
        if !cancelled && (accepted || accept_requested) {
            if was_confirming {
                model.confirm_replace();
            } else {
                model.choose();
            }
        }
        let confirmation_changed = was_confirming != model.confirming_replace();
        if confirmation_changed {
            self.focus = Focus::new();
            self.focus.set(0);
            ui.input.clear_accessible_text_edit();
        }
        self.input.end_frame();
        if listing_is_stale || model.exit || confirmation_changed {
            self.semantics.invalidate();
        }
    }
}

struct DialogApp {
    model: Model,
    surface: Surface,
    answer: Option<tokio::sync::oneshot::Sender<crate::host::UiResult<Answer>>>,
    abandoned: Arc<std::sync::atomic::AtomicBool>,
    close_requested: bool,
    waker: Option<pixelkit_windowing::Waker>,
    appearance: Option<templar_client::appearance::AppearanceObserver>,
    parent_binding: Option<crate::parent::Binding>,
}

impl PixelApp for DialogApp {
    fn on_layout_changed(&mut self) {
        self.surface.input.clear_accessible_text_edit();
        self.surface.semantics.invalidate();
    }
    fn attach(&mut self, waker: pixelkit_windowing::Waker) {
        self.waker = Some(waker.clone());
        let appearance_waker = waker.clone();
        self.appearance = Some(templar_client::appearance::AppearanceObserver::new(
            move || appearance_waker.wake(),
        ));
        self.surface
            .input
            .attach_clipboard(pixelkit_shell::clipboard::ClipboardWorker::new(move || {
                waker.wake()
            }));
    }
    fn on_native_window(&mut self, window: &winit::window::Window) {
        self.parent_binding = None;
        let Some(waker) = &self.waker else {
            return;
        };
        match crate::parent::Binding::connect(&self.model.parent, window, waker.clone()) {
            Ok(binding) => {
                self.model.parent_notice = Some(
                    if binding.is_none() {
                        "No parent supplied; independent dialog."
                    } else {
                        "Linking requesting window; no activation inferred."
                    }
                    .into(),
                );
                self.parent_binding = binding;
            }
            Err(error) => {
                tracing::warn!(%error, "native parent unavailable; keeping visible independent dialog");
                self.model.parent_notice = Some(error.to_string());
            }
        }
    }
    fn render(&mut self, buffer: &mut WindowBuffer, scale: Scale) {
        if let Some(binding) = &mut self.parent_binding {
            match binding.poll() {
                Ok(true) => self.model.parent_notice = None,
                Ok(false) => {}
                Err(notice) => {
                    self.parent_binding = None;
                    self.model.parent_notice = Some(notice.into());
                    tracing::warn!(
                        notice,
                        "native parent unavailable; dialog remains reachable without activation grant"
                    );
                }
            }
        }
        if let Some(update) = self
            .appearance
            .as_mut()
            .and_then(|observer| observer.take_update())
        {
            match update {
                Ok(appearance) => self.surface.theme = templar_ui::theme_for(&appearance.palette),
                Err(error) => {
                    self.model.error = Some(format!("Appearance observation unavailable: {error}"))
                }
            }
        }
        let changed =
            !self.surface.input.keys().is_empty() || self.surface.input.press_position().is_some();
        self.surface.draw(buffer, scale, &mut self.model);
        if changed {
            if let Some(waker) = &self.waker {
                waker.wake();
            }
        }
    }
    fn on_key(&mut self, key: &KeyInput) {
        if *key == KeyInput::Escape {
            self.model.cancel();
            self.surface.focus = Focus::new();
            self.surface.semantics.invalidate();
        } else if !matches!(key, KeyInput::Character(_)) {
            self.surface.input.key(key.clone());
        }
    }
    fn on_key_event(&mut self, event: &pixelkit_windowing::KeyEvent) {
        if event.pressed && matches!(event.key, KeyInput::Character(_)) {
            if event.modifiers.control || event.modifiers.logo {
                self.surface.input.key(event.key.clone());
            } else {
                for character in event.text.chars() {
                    self.surface.input.key(KeyInput::Character(character));
                }
            }
        }
    }
    fn on_ime(&mut self, event: &pixelkit_windowing::ImeEvent) {
        match event {
            pixelkit_windowing::ImeEvent::Preedit(text, _) => {
                self.surface.input.set_composition(text)
            }
            pixelkit_windowing::ImeEvent::Commit(text) => {
                self.surface.input.set_composition("");
                for character in text.chars() {
                    self.surface.input.key(KeyInput::Character(character));
                }
            }
            pixelkit_windowing::ImeEvent::Disabled => self.surface.input.set_composition(""),
            pixelkit_windowing::ImeEvent::Enabled => {}
        }
    }
    fn on_focus_changed(&mut self, focused: bool) {
        if !focused {
            self.surface.input.blur();
            self.surface.semantics.invalidate();
        }
    }
    fn ime_allowed(&self) -> bool {
        self.ime_cursor_area().is_some()
    }
    fn ime_cursor_area(&self) -> Option<pixelkit_windowing::ImeCursorArea> {
        let Mode::Files(files) = &self.model.mode else {
            return None;
        };
        if files.replace.is_some() {
            return None;
        }
        let area = files.name.caret_area()?;
        Some(pixelkit_windowing::ImeCursorArea {
            x: area.x as f64,
            y: area.y as f64,
            width: area.w as f64,
            height: area.h as f64,
        })
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
    fn poll_window(&mut self) -> Option<WindowRequest> {
        if !self.close_requested
            && (self.model.exit
                || self.abandoned.load(Ordering::Acquire)
                || self
                    .answer
                    .as_ref()
                    .is_some_and(|answer| answer.is_closed()))
        {
            self.close_requested = true;
            return Some(WindowRequest::Close);
        }
        None
    }
    fn on_exit(&mut self) {
        self.parent_binding = None;
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(Ok(self.model.answer.take()));
        }
    }
    fn on_host_error(&mut self, error: &str) {
        self.parent_binding = None;
        self.model.exit = true;
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(Err(crate::host::UiError(error.to_owned())));
        }
    }
    fn accessibility_tree(&mut self) -> Option<pixelkit_ui::semantics::TreeUpdate> {
        self.surface.semantics_active = true;
        Some(self.surface.semantics.update())
    }
    fn on_accessibility_action(&mut self, request: pixelkit_ui::semantics::ActionRequest) {
        self.surface
            .semantics
            .action(&mut self.surface.input, request);
    }
    fn on_accessibility_deactivated(&mut self) {
        self.surface.input.clear_accessible_text_edit();
        self.surface.semantics.invalidate();
        self.surface.semantics_active = false;
    }
}

pub fn application(pending: crate::host::Pending) -> Box<dyn PixelApp> {
    Box::new(DialogApp {
        model: pending.model,
        surface: Surface::new(),
        answer: Some(pending.answer),
        abandoned: pending.abandoned,
        close_requested: false,
        waker: None,
        appearance: None,
        parent_binding: None,
    })
}

pub fn choice(title: String, message: String, choices: Vec<String>) -> Model {
    Model {
        title,
        mode: Mode::Choice {
            message,
            choices,
            selected: 0,
        },
        answer: None,
        exit: false,
        error: None,
        activation_token: None,
        parent: Default::default(),
        parent_notice: None,
    }
}

pub fn files(request: ChooserRequest) -> Model {
    let initial_name = match &request.kind {
        ChooserKind::Save { current_name } => current_name.clone().unwrap_or_default(),
        ChooserKind::Open { .. } | ChooserKind::SaveFiles { .. } => String::new(),
    };
    let mut browser = Browser::new(
        request.kind,
        request.starting_directory,
        request.filters,
        request.current_filter,
    );
    browser.refresh();
    Model {
        title: request.title,
        mode: Mode::Files(FileMode {
            cursor: 0,
            browser,
            scroll: ScrollState::new(),
            filter_menu: Dropdown::new(),
            name: TextFieldState::with_text(initial_name),
            replace: None,
        }),
        answer: None,
        exit: false,
        error: None,
        activation_token: request.activation_token,
        parent: request.parent,
        parent_notice: None,
    }
}

fn theme() -> Theme {
    templar_ui::theme_for(&templar_types::appearance::LIGHT)
}

fn fonts() -> &'static FontSet {
    static FONTS: std::sync::OnceLock<FontSet> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        let face = Box::leak(Box::new(
            Face::from_bytes(include_bytes!(
                "../../../shell/templar-shell/assets/fonts/DejaVuSans.ttf"
            ))
            .expect("embedded DejaVu Sans"),
        ));
        let mut fonts = FontSet::new();
        fonts.add(face);
        fonts
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "templar-chooser-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("create test directory: {error}"),
                }
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn save_model(directory: &TestDirectory, name: &str) -> Model {
        files(ChooserRequest {
            kind: ChooserKind::Save {
                current_name: Some(name.into()),
            },
            app_id: "org.templaros.Test".into(),
            title: "Save".into(),
            activation_token: None,
            parent: Default::default(),
            starting_directory: directory.0.clone(),
            filters: vec![
                FileFilter {
                    name: "Page".into(),
                    patterns: vec![FilterPattern::Glob("*.html".into())],
                },
                FileFilter {
                    name: "Text".into(),
                    patterns: vec![FilterPattern::Mime("text/plain".into())],
                },
            ],
            current_filter: Some(0),
        })
    }

    fn file_mode(model: &mut Model) -> &mut FileMode {
        let Mode::Files(files) = &mut model.mode else {
            panic!("file mode")
        };
        files
    }

    #[test]
    fn existing_save_requires_confirmation_and_cancel_allows_editing() {
        let directory = TestDirectory::new();
        let target = directory.0.join("existing.html");
        std::fs::write(&target, b"original").unwrap();
        let mut model = save_model(&directory, "existing.html");
        file_mode(&mut model).browser.set_current_filter(Some(1));
        model.choose();
        assert!(model.confirming_replace());
        assert!(model.answer.is_none());
        assert!(!model.exit);
        model.choose();
        assert!(
            model.answer.is_none(),
            "Choose cannot substitute for Replace"
        );
        model.cancel();
        assert!(!model.confirming_replace());
        assert!(model.answer.is_none());
        assert!(!model.exit);
        assert_eq!(file_mode(&mut model).name.text(), "existing.html");
        assert_eq!(file_mode(&mut model).browser.current_filter(), Some(1));
        assert_eq!(std::fs::read(&target).unwrap(), b"original");

        file_mode(&mut model).name.set_text("new.txt");
        model.choose();
        let Some(Answer::Files(selection)) = &model.answer else {
            panic!("save answer")
        };
        assert!(model.exit);
        assert_eq!(selection.paths, [directory.0.join("new.txt")]);
        assert_eq!(
            selection.current_filter.as_ref().unwrap().patterns,
            [FilterPattern::Mime("text/plain".into())]
        );
        assert!(
            !directory.0.join("new.txt").exists(),
            "the picker never writes the save"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }

    #[test]
    fn deliberate_replace_returns_target_without_touching_it() {
        let directory = TestDirectory::new();
        let target = directory.0.join("existing.html");
        std::fs::write(&target, b"original").unwrap();
        let mut model = save_model(&directory, "existing.html");
        model.choose();
        assert!(model.confirming_replace());
        model.confirm_replace();
        assert!(model.exit);
        let Some(Answer::Files(selection)) = model.answer else {
            panic!("save answer")
        };
        assert_eq!(selection.paths, [target.clone()]);
        assert_eq!(std::fs::read(target).unwrap(), b"original");
    }

    #[test]
    fn cancelling_picker_after_declining_replace_returns_no_success() {
        let directory = TestDirectory::new();
        let target = directory.0.join("existing.html");
        std::fs::write(&target, b"original").unwrap();
        let mut model = save_model(&directory, "existing.html");
        model.choose();
        model.cancel();
        model.cancel();
        assert!(model.exit);
        assert!(model.answer.is_none());
        assert_eq!(std::fs::read(target).unwrap(), b"original");
    }

    #[test]
    fn all_files_selection_is_returned_instead_of_the_initial_format() {
        let directory = TestDirectory::new();
        let mut model = save_model(&directory, "new.html");
        file_mode(&mut model).browser.set_current_filter(None);
        model.choose();
        let Some(Answer::Files(selection)) = model.answer else {
            panic!("save answer")
        };
        assert_eq!(
            selection.current_filter,
            Some(FileFilter {
                name: "All files".into(),
                patterns: vec![FilterPattern::Glob("*".into())],
            })
        );
    }

    #[test]
    fn save_cannot_return_a_directory_as_a_file() {
        let directory = TestDirectory::new();
        std::fs::create_dir(directory.0.join("folder")).unwrap();
        let mut model = save_model(&directory, "folder");
        model.choose();
        assert!(model.answer.is_none());
        assert!(!model.exit);
        assert!(!model.confirming_replace());
        assert!(model.error.is_some());
    }
}
