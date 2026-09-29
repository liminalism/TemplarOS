//! Small native portal dialogs hosted with Pixelkit. They run on blocking worker threads so a
//! modal window never stalls the portal's D-Bus executor.
//!
//! This module is deliberately thin: *what* the file dialog is showing, and what a click means,
//! is [`crate::browser::Browser`], which has no display in it and is tested. What is left here
//! is the drawing — a virtualised list so a directory of any size scrolls, a hidden-file
//! checkbox, a filter menu, and a name field when saving.

use crate::{
    browser::{Activation, Browser},
    file_chooser::{ChooserKind, ChooserRequest, ChosenFiles},
};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, Modifiers, MouseButton, Scale};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{Dropdown, ScrollState, TextFieldState, Theme, Ui};
use pixelkit_windowing::{PixelApp, WindowConfig, run_app};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
enum Answer {
    Choice(String),
    Files(Vec<PathBuf>),
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
}

#[derive(Debug)]
struct Model {
    title: String,
    mode: Mode,
    answer: Arc<Mutex<Option<Answer>>>,
    exit: bool,
    error: Option<String>,
}

impl Model {
    fn choose(&mut self) {
        let answer = match &mut self.mode {
            Mode::Choice {
                choices, selected, ..
            } => choices.get(*selected).cloned().map(Answer::Choice),
            Mode::Files(files) => match files.browser.choose(files.name.text()) {
                Ok(paths) => Some(Answer::Files(paths)),
                Err(message) => {
                    self.error = Some(message.to_owned());
                    return;
                }
            },
        };
        if let Some(answer) = answer {
            *self.answer.lock().expect("dialog answer") = Some(answer);
            self.exit = true;
        }
    }
}

/// Draw the body for whichever mode this dialog is in. Returns whether the listing must be
/// re-read once `mode` is free again (the browser is borrowed for the whole of the drawing).
fn draw_mode(ui: &mut Ui<'_>, area: Rect, mode: &mut Mode) -> bool {
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
            ui.radio_group(
                selected,
                Rect::new(area.x, area.y + ui.px(92), area.w, ui.px(260)),
                &labels,
                true,
            );
            false
        }
        Mode::Files(files) => draw_files(ui, area, files),
    }
}

/// The file browser: a parent button and the current path, the hidden/filter controls, the
/// scrolling listing, and a name field when saving. Returns whether the listing must be re-read.
fn draw_files(ui: &mut Ui<'_>, area: Rect, files: &mut FileMode) -> bool {
    let mut stale = false;

    if ui.button(
        Rect::new(area.x, area.y + ui.px(50), ui.px(54), ui.px(34)),
        "Up",
    ) && files.browser.go_up()
    {
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

    stale |= draw_controls(ui, area, files);

    let saving = matches!(
        files.browser.kind(),
        ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. }
    );
    let list_bottom = area.bottom() - if saving { ui.px(104) } else { ui.px(56) };
    let list = Rect::new(
        area.x,
        area.y + ui.px(132),
        area.w,
        (list_bottom - (area.y + ui.px(132))).max(ui.px(30)),
    );
    if let Some(activated) = draw_listing(ui, list, files) {
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
        ui.text_field(
            &mut files.name,
            Rect::new(area.x, area.bottom() - ui.px(96), area.w, ui.px(38)),
            "File name",
            true,
            false,
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
fn draw_controls(ui: &mut Ui<'_>, area: Rect, files: &mut FileMode) -> bool {
    let mut show_hidden = files.browser.show_hidden();
    let toggled = ui.checkbox(
        Rect::new(area.x, area.y + ui.px(94), ui.px(160), ui.px(28)),
        &mut show_hidden,
        "Show hidden files",
    );
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
        ui.dropdown(
            &mut files.filter_menu,
            Rect::new(
                area.right() - ui.px(220),
                area.y + ui.px(92),
                ui.px(220),
                ui.px(32),
            ),
            &borrowed,
            selected,
        );
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
fn draw_listing(ui: &mut Ui<'_>, area: Rect, files: &mut FileMode) -> Option<usize> {
    let visible = files.browser.visible();
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
                format!("[{}]", entry.name)
            } else if files.browser.is_selected(&entry.path) {
                format!("✓ {}", entry.name)
            } else {
                entry.name.clone()
            }
        })
        .collect();
    let row = ui.px(34);
    let heights = vec![row; labels.len()];
    let mut activated = None;
    ui.scroll_list(&mut files.scroll, area, &heights, |ui, index, rect| {
        let inner = Rect::new(rect.x, rect.y, rect.w, rect.h - ui.px(4));
        if ui.button(inner, &labels[index]) {
            activated = Some(index);
        }
    });
    activated
}

struct Surface {
    input: Input,
    text: TextCache,
    kernel: RasterKernel,
}

impl Surface {
    fn new() -> Self {
        Self {
            input: Input::new(),
            text: TextCache::new(fonts()),
            kernel: RasterKernel::new(),
        }
    }

    fn draw(&mut self, buffer: &mut WindowBuffer, scale: Scale, model: &mut Model) {
        let whole = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
        let mut ui = Ui::new(
            Painter::new(buffer),
            &mut self.text,
            &mut self.input,
            theme(),
            scale,
            &mut self.kernel,
        );
        ui.painter.fill_rect(whole, ui.theme.background);
        let area = whole.inset(ui.px(20));
        ui.heading(Rect::new(area.x, area.y, area.w, ui.px(44)), &model.title);
        // The directory listing is re-read after the match: `model.mode` is borrowed for the
        // whole of it, so navigation only records that a refresh is due.
        let listing_is_stale = draw_mode(&mut ui, area, &mut model.mode);
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
        if ui.button(cancel, "Cancel") {
            model.exit = true;
        }
        if ui.button(accept, "Choose") {
            model.choose();
        }
        self.input.end_frame();
    }
}

struct DialogApp {
    model: Model,
    surface: Surface,
}

impl PixelApp for DialogApp {
    fn render(&mut self, buffer: &mut WindowBuffer, scale: Scale) {
        self.surface.draw(buffer, scale, &mut self.model);
    }
    fn on_key(&mut self, key: &KeyInput) {
        if *key == KeyInput::Escape {
            self.model.exit = true;
        } else if *key == KeyInput::Enter {
            self.model.choose();
        } else {
            self.surface.input.key(key.clone());
        }
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
    fn should_exit(&self) -> bool {
        self.model.exit
    }
}

fn run(model: Model) -> Option<Answer> {
    let answer = Arc::clone(&model.answer);
    let app = DialogApp {
        model,
        surface: Surface::new(),
    };
    if let Err(error) = run_app(
        app,
        WindowConfig::new("TemplarOS portal", 720.0, 560.0).min_size(560.0, 440.0),
    ) {
        tracing::error!(%error, "portal dialog failed");
        return None;
    }
    answer.lock().expect("dialog answer").clone()
}

pub fn choice(title: String, message: String, choices: Vec<String>) -> Option<String> {
    if choices.is_empty() {
        return None;
    }
    let answer = Arc::new(Mutex::new(None));
    match run(Model {
        title,
        mode: Mode::Choice {
            message,
            choices,
            selected: 0,
        },
        answer,
        exit: false,
        error: None,
    }) {
        Some(Answer::Choice(choice)) => Some(choice),
        _ => None,
    }
}

pub fn files(request: ChooserRequest) -> Option<ChosenFiles> {
    let initial_name = match &request.kind {
        ChooserKind::Save { current_name } => current_name.clone().unwrap_or_default(),
        ChooserKind::SaveFiles { file_names } => file_names.first().cloned().unwrap_or_default(),
        ChooserKind::Open { .. } => String::new(),
    };
    let mut browser = Browser::new(
        request.kind,
        request.starting_directory,
        request.filters,
        request.current_filter,
    );
    browser.refresh();
    let model = Model {
        title: request.title,
        mode: Mode::Files(FileMode {
            browser,
            scroll: ScrollState::new(),
            filter_menu: Dropdown::new(),
            name: TextFieldState::with_text(initial_name),
        }),
        answer: Arc::new(Mutex::new(None)),
        exit: false,
        error: None,
    };
    match run(model) {
        Some(Answer::Files(paths)) => Some(ChosenFiles {
            uris: paths
                .into_iter()
                .filter_map(|path| url::Url::from_file_path(path).ok())
                .map(Into::into)
                .collect(),
            writable: true,
        }),
        _ => None,
    }
}

fn theme() -> Theme {
    let mut theme = Theme::default();
    theme.background = 0x00ec_eef0;
    theme.panel = 0x00fb_fcfd;
    theme.panel_edge = 0x00dd_e1e6;
    theme.text = 0x0017_1b20;
    theme.text_dim = 0x0069_737d;
    theme.accent = 0x000e_7482;
    theme.accent_soft = 0x00e3_f1f3;
    theme.hover = 0x00ff_ffff;
    theme.selection = theme.accent_soft;
    theme.body = theme.body.with_size(14.0);
    theme.heading = theme.heading.with_size(28.0);
    theme
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
