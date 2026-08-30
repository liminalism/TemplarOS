//! Graphical Settings surface. The Home Field form is generated from `Settings1.SchemaDetails`;
//! bounds and choices therefore remain service policy rather than becoming UI constants.

use anyhow::{Context, Result};
use lege_client::proxies::SettingsProxy;
use lege_session_core::settings::{SettingSpec, SettingValue};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{
    Input, KeyInput, Modifiers, MouseButton, PixelApp, Scale, Waker, WindowConfig, run_app,
};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{Theme, Ui};
use std::sync::{Arc, Mutex, mpsc};

const HOME_PREFIX: &str = "shell.home-field.";
const PAGE_NAMES: [&str; 7] = [
    "Home Field",
    "Providers",
    "Associations",
    "Background",
    "Resources",
    "History",
    "Hardware",
];

#[derive(Clone, Debug)]
struct SettingRow {
    spec: SettingSpec,
    value: SettingValue,
}

#[derive(Debug)]
enum Command {
    Set { key: String, value: SettingValue },
    Reset { key: String },
}

#[derive(Debug)]
struct ResultMessage {
    key: String,
    result: Result<(), String>,
}

#[derive(Debug)]
struct Backend {
    commands: mpsc::Sender<Command>,
    results: mpsc::Receiver<ResultMessage>,
    waker: Arc<Mutex<Waker>>,
}

impl Backend {
    fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let waker = Arc::new(Mutex::new(Waker::detached()));
        let worker_waker = Arc::clone(&waker);
        std::thread::Builder::new()
            .name("lege-settings-writes".into())
            .spawn(move || {
                let runtime = tokio::runtime::Runtime::new().expect("settings write runtime");
                while let Ok(command) = command_rx.recv() {
                    let command_key = match &command {
                        Command::Set { key, .. } | Command::Reset { key } => key.clone(),
                    };
                    let result = runtime.block_on(async {
                        let connection =
                            zbus::Connection::session().await.context("session bus")?;
                        let proxy = SettingsProxy::new(&connection).await?;
                        match command {
                            Command::Set { key, value } => {
                                let json = serde_json::to_string(&value)?;
                                proxy.set(&key, &json, "lege-settings-gui").await?;
                                Ok::<_, anyhow::Error>((key, ()))
                            }
                            Command::Reset { key } => {
                                proxy.reset(&key, "lege-settings-gui").await?;
                                Ok((key, ()))
                            }
                        }
                    });
                    let message = match result {
                        Ok((key, ())) => ResultMessage {
                            key,
                            result: Ok(()),
                        },
                        Err(error) => ResultMessage {
                            key: command_key,
                            result: Err(format!("{error:#}")),
                        },
                    };
                    let _ = result_tx.send(message);
                    worker_waker.lock().expect("waker mutex").wake();
                }
            })
            .expect("spawn settings writer");
        Self {
            commands: command_tx,
            results: result_rx,
            waker,
        }
    }
}

struct SettingsApp {
    home: Vec<SettingRow>,
    pages: Vec<Vec<String>>,
    selected_page: usize,
    input: Input,
    text: TextCache,
    kernel: RasterKernel,
    backend: Backend,
    status: String,
}

impl SettingsApp {
    fn change(&mut self, index: usize, value: SettingValue) {
        self.home[index].value = value.clone();
        let key = self.home[index].spec.key.clone();
        self.status = format!("Saving {key}…");
        let _ = self.backend.commands.send(Command::Set { key, value });
    }

    fn reset(&mut self, index: usize) {
        self.home[index].value = self.home[index].spec.default.clone();
        let key = self.home[index].spec.key.clone();
        self.status = format!("Resetting {key}…");
        let _ = self.backend.commands.send(Command::Reset { key });
    }

    fn drain_results(&mut self) {
        while let Ok(message) = self.backend.results.try_recv() {
            self.status = match message.result {
                Ok(()) => format!("Saved {}", message.key),
                Err(error) => format!("Could not save {} — {error}", message.key),
            };
        }
    }

    fn draw_home(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let mut y = area.y;
        for index in 0..self.home.len() {
            let row_area = Rect::new(area.x, y, area.w, ui.px(78));
            ui.painter
                .fill_rounded_rect(row_area, ui.px(6), 0x00fb_fcfd);
            ui.painter
                .stroke_rect(row_area, ui.scale.hairline(), 0x00dd_e1e6);
            let name = self.home[index]
                .spec
                .key
                .strip_prefix(HOME_PREFIX)
                .unwrap_or(&self.home[index].spec.key)
                .replace('-', " ");
            ui.label(
                Rect::new(
                    row_area.x + ui.px(12),
                    row_area.y + ui.px(7),
                    row_area.w - ui.px(220),
                    ui.px(24),
                ),
                &name,
                ui.theme.text,
                Align::Left,
            );
            ui.label(
                Rect::new(
                    row_area.x + ui.px(12),
                    row_area.y + ui.px(33),
                    row_area.w - ui.px(220),
                    ui.px(34),
                ),
                &self.home[index].spec.description,
                ui.theme.text_dim,
                Align::Left,
            );
            let controls = Rect::new(
                row_area.right() - ui.px(196),
                row_area.y + ui.px(20),
                ui.px(184),
                ui.px(38),
            );
            let reset = Rect::new(controls.x - ui.px(66), controls.y, ui.px(58), controls.h);
            if ui.button(reset, "Reset") {
                self.reset(index);
            }
            self.draw_home_control(ui, index, controls);
            y += ui.px(88);
        }
    }

    /// The editing control for one Home setting, chosen by the value's type: a toggle for a flag,
    /// a stepper clamped to the spec's bounds for a number, and a cycling button for a choice.
    fn draw_home_control(&mut self, ui: &mut Ui<'_>, index: usize, controls: Rect) {
        match self.home[index].value.clone() {
            SettingValue::Bool(mut enabled) => {
                let label = if enabled { "On" } else { "Off" };
                if ui.toggle(controls, &mut enabled, label) {
                    self.change(index, SettingValue::Bool(enabled));
                }
            }
            SettingValue::Int(value) => {
                let minus = Rect::new(controls.x, controls.y, ui.px(38), controls.h);
                let plus = Rect::new(
                    controls.right() - ui.px(38),
                    controls.y,
                    ui.px(38),
                    controls.h,
                );
                if ui.button(minus, "−") {
                    let min = self.home[index].spec.min.unwrap_or(i64::MIN);
                    self.change(index, SettingValue::Int(value.saturating_sub(1).max(min)));
                }
                ui.label(
                    Rect::new(
                        minus.right(),
                        controls.y,
                        controls.w - ui.px(76),
                        controls.h,
                    ),
                    &value.to_string(),
                    ui.theme.text,
                    Align::Centre,
                );
                if ui.button(plus, "+") {
                    let max = self.home[index].spec.max.unwrap_or(i64::MAX);
                    self.change(index, SettingValue::Int(value.saturating_add(1).min(max)));
                }
            }
            SettingValue::Choice(value) => {
                let choices = &self.home[index].spec.choices;
                if ui.button(controls, &value) && !choices.is_empty() {
                    let position = choices
                        .iter()
                        .position(|choice| choice == &value)
                        .unwrap_or(0);
                    let next = choices[(position + 1) % choices.len()].clone();
                    self.change(index, SettingValue::Choice(next));
                }
            }
            other => ui.label(
                controls,
                &format!("{other:?}"),
                ui.theme.text,
                Align::Centre,
            ),
        }
    }

    fn draw_text_page(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let lines = self
            .pages
            .get(self.selected_page)
            .cloned()
            .unwrap_or_default();
        for (index, line) in lines.iter().take(20).enumerate() {
            ui.label(
                Rect::new(area.x, area.y + ui.px(index as i32 * 26), area.w, ui.px(24)),
                line,
                if line.ends_with(':') {
                    ui.theme.text
                } else {
                    ui.theme.text_dim
                },
                Align::Left,
            );
        }
    }
}

impl PixelApp for SettingsApp {
    fn render(&mut self, buffer: &mut WindowBuffer, scale: Scale) {
        self.drain_results();
        let whole = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
        let mut text = std::mem::replace(&mut self.text, TextCache::new(fonts()));
        let mut input = std::mem::replace(&mut self.input, Input::new());
        let mut kernel = std::mem::replace(&mut self.kernel, RasterKernel::new());
        {
            let mut ui = Ui::new(
                Painter::new(buffer),
                &mut text,
                &mut input,
                theme(),
                scale,
                &mut kernel,
            );
            ui.painter.fill_rect(whole, ui.theme.background);
            let inset = whole.inset(ui.px(18));
            ui.heading(Rect::new(inset.x, inset.y, inset.w, ui.px(48)), "Settings");
            let tabs = Rect::new(inset.x, inset.y + ui.px(56), inset.w, ui.px(42));
            ui.tabs(&mut self.selected_page, tabs, &PAGE_NAMES);
            let content = Rect::new(
                inset.x,
                tabs.bottom() + ui.px(14),
                inset.w,
                inset.bottom() - tabs.bottom() - ui.px(52),
            );
            if self.selected_page == 0 {
                self.draw_home(&mut ui, content);
            } else {
                self.draw_text_page(&mut ui, content);
            }
            ui.label(
                Rect::new(inset.x, inset.bottom() - ui.px(34), inset.w, ui.px(28)),
                &self.status,
                ui.theme.text_dim,
                Align::Left,
            );
            input.end_frame();
        }
        self.text = text;
        self.input = input;
        self.kernel = kernel;
    }

    fn attach(&mut self, waker: Waker) {
        *self.backend.waker.lock().expect("waker mutex") = waker;
    }

    fn tick(&mut self) {
        self.drain_results();
    }

    fn on_key(&mut self, key: &KeyInput) {
        self.input.key(key.clone());
    }
    fn on_modifiers(&mut self, modifiers: Modifiers) {
        self.input.modifiers_changed(modifiers);
    }
    fn on_cursor(&mut self, x: f32, y: f32) {
        self.input.cursor_moved(x, y);
    }
    fn on_cursor_left(&mut self) {
        self.input.cursor_left();
    }
    fn on_mouse(&mut self, button: MouseButton, pressed: bool) {
        self.input.mouse(button, pressed);
    }
    fn on_scroll(&mut self, dx: f32, dy: f32) {
        self.input.scrolled(dx, dy);
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
    theme.body = theme.body.with_size(13.0);
    theme.heading = theme.heading.with_size(32.0);
    theme.padding = 12;
    theme.corner_radius = 7;
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

async fn load() -> Result<SettingsApp> {
    let connection = zbus::Connection::session().await.context("session bus")?;
    let proxy = SettingsProxy::new(&connection).await?;
    let values: std::collections::HashMap<_, _> = proxy.list().await?.into_iter().collect();
    let mut home = Vec::new();
    for json in proxy.schema_details().await? {
        let spec: SettingSpec = serde_json::from_str(&json)?;
        if !spec.key.starts_with(HOME_PREFIX) {
            continue;
        }
        let value = values
            .get(&spec.key)
            .map(|json| serde_json::from_str(json))
            .transpose()?
            .unwrap_or_else(|| spec.default.clone());
        home.push(SettingRow { spec, value });
    }
    home.sort_by(|left, right| left.spec.key.cmp(&right.spec.key));

    let pages = vec![
        Vec::new(),
        super::providers::render(&connection, None)
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        super::associations::render(&connection, None, "")
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        super::background::render(&connection)
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        super::resources::render(&connection, 20)
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        super::history::render(&connection, None, 20, None)
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        super::hardware::render()
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
    ];
    Ok(SettingsApp {
        home,
        pages,
        selected_page: 0,
        input: Input::new(),
        text: TextCache::new(fonts()),
        kernel: RasterKernel::new(),
        backend: Backend::new(),
        status: "Changes are validated and recorded by Settings1".into(),
    })
}

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("settings startup runtime")?;
    let app = runtime.block_on(load())?;
    drop(runtime);
    run_app(
        app,
        WindowConfig::new("Settings", 1040.0, 680.0).min_size(820.0, 600.0),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
