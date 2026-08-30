//! Small native portal dialogs hosted with Pixelkit. They run on blocking worker threads so a
//! modal window never stalls the portal's D-Bus executor.

use crate::file_chooser::{ChooserKind, ChooserRequest, ChosenFiles};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{
    Input, KeyInput, Modifiers, MouseButton, PixelApp, Scale, WindowConfig, run_app,
};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{TextFieldState, Theme, Ui};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
enum Answer {
    Choice(String),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Debug)]
enum Mode {
    Choice {
        message: String,
        choices: Vec<String>,
        selected: usize,
    },
    Files {
        kind: ChooserKind,
        directory: PathBuf,
        entries: Vec<PathBuf>,
        selected: Vec<PathBuf>,
        path: TextFieldState,
    },
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
    fn refresh_entries(&mut self) {
        let Mode::Files {
            directory, entries, ..
        } = &mut self.mode
        else {
            return;
        };
        let mut found: Vec<_> = std::fs::read_dir(&*directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        found.sort_by_key(|path| {
            (
                !path.is_dir(),
                path.file_name().map(std::ffi::OsStr::to_os_string),
            )
        });
        found.truncate(10);
        *entries = found;
    }

    fn choose(&mut self) {
        let answer = match &self.mode {
            Mode::Choice {
                choices, selected, ..
            } => choices.get(*selected).cloned().map(Answer::Choice),
            Mode::Files {
                kind,
                directory,
                selected,
                path,
                ..
            } => {
                let paths = match kind {
                    ChooserKind::Open {
                        directory: true, ..
                    } => vec![directory.clone()],
                    ChooserKind::Open { .. } if selected.is_empty() => {
                        self.error = Some("Choose at least one file.".into());
                        return;
                    }
                    ChooserKind::Open {
                        multiple: false, ..
                    } => vec![selected[0].clone()],
                    ChooserKind::Open { multiple: true, .. } => selected.clone(),
                    ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. } => {
                        let candidate = PathBuf::from(path.text());
                        if candidate.as_os_str().is_empty() {
                            self.error = Some("Enter a file name.".into());
                            return;
                        }
                        vec![if candidate.is_absolute() {
                            candidate
                        } else {
                            directory.join(candidate)
                        }]
                    }
                };
                Some(Answer::Files(paths))
            }
        };
        if let Some(answer) = answer {
            *self.answer.lock().expect("dialog answer") = Some(answer);
            self.exit = true;
        }
    }
}

/// Draw the body for whichever mode this dialog is in. Returns whether the user navigated to a
/// different directory, which the caller turns into a listing refresh once `mode` is free again.
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
        Mode::Files {
            kind,
            directory,
            entries,
            selected,
            path,
        } => draw_files(ui, area, kind, directory, entries, selected, path),
    }
}

/// The file browser: a parent button, the current directory, one row per entry, and a name field
/// when saving. Returns whether `directory` changed.
fn draw_files(
    ui: &mut Ui<'_>,
    area: Rect,
    kind: &ChooserKind,
    directory: &mut PathBuf,
    entries: &[PathBuf],
    selected: &mut Vec<PathBuf>,
    path: &mut TextFieldState,
) -> bool {
    let mut navigated = false;
    let directory_label = directory.display().to_string();
    if ui.button(
        Rect::new(area.x, area.y + ui.px(50), ui.px(54), ui.px(34)),
        "Up",
    ) && let Some(parent) = directory.parent()
    {
        *directory = parent.to_path_buf();
        navigated = true;
    }
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
    for (index, entry) in entries.iter().enumerate() {
        let row = Rect::new(
            area.x,
            area.y + ui.px(94 + index as i32 * 34),
            area.w,
            ui.px(30),
        );
        let name = entry.file_name().map_or_else(
            || entry.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let label = if entry.is_dir() {
            format!("[{name}]")
        } else if selected.contains(entry) {
            format!("✓ {name}")
        } else {
            name
        };
        if ui.button(row, &label) {
            if entry.is_dir() {
                directory.clone_from(entry);
                selected.clear();
                navigated = true;
            } else {
                pick(kind, selected, path, entry);
            }
        }
    }
    if matches!(
        kind,
        ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. }
    ) {
        ui.text_field(
            path,
            Rect::new(area.x, area.bottom() - ui.px(92), area.w, ui.px(38)),
            "File name",
            true,
            false,
        );
    }
    navigated
}

/// Apply a click on a non-directory `entry`: multi-select toggles, single-select replaces, and a
/// save dialog copies the name into its field.
fn pick(
    kind: &ChooserKind,
    selected: &mut Vec<PathBuf>,
    path: &mut TextFieldState,
    entry: &PathBuf,
) {
    match kind {
        ChooserKind::Open { multiple: true, .. } => {
            if let Some(at) = selected.iter().position(|path| path == entry) {
                selected.remove(at);
            } else {
                selected.push(entry.clone());
            }
        }
        ChooserKind::Open { .. } => {
            selected.clear();
            selected.push(entry.clone());
        }
        ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. } => {
            path.set_text(entry.display().to_string());
        }
    }
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
        let navigated = draw_mode(&mut ui, area, &mut model.mode);
        if navigated {
            model.refresh_entries();
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
        WindowConfig::new("LegeOS portal", 720.0, 560.0).min_size(560.0, 440.0),
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
    let answer = Arc::new(Mutex::new(None));
    let mut model = Model {
        title: request.title,
        mode: Mode::Files {
            kind: request.kind,
            directory: request.starting_directory,
            entries: Vec::new(),
            selected: Vec::new(),
            path: TextFieldState::with_text(initial_name),
        },
        answer,
        exit: false,
        error: None,
    };
    model.refresh_entries();
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
                "../../../shell/lege-shell/assets/fonts/DejaVuSans.ttf"
            ))
            .expect("embedded DejaVu Sans"),
        ));
        let mut fonts = FontSet::new();
        fonts.add(face);
        fonts
    })
}
