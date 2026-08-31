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
/// The two interactive pages come first and the rendered text pages follow. `System` is second
/// because it is the page the Home Field's Settings widget opens into: the widget reports the
/// battery, the volume and the brightness, and this is where they can be set outright.
const PAGE_NAMES: [&str; 8] = [
    "Home Field",
    "System",
    "Providers",
    "Associations",
    "Background",
    "Resources",
    "History",
    "Hardware",
];
/// The index of each page that draws controls rather than rendered text.
const HOME_PAGE: usize = 0;
const SYSTEM_PAGE: usize = 1;
/// How far one press of a stepper moves a level: the same five points `lege status louder` and
/// the Home Field's own controls use, so no two surfaces disagree about what one step means.
const STEP_PERCENT: i32 = 5;

#[derive(Clone, Debug)]
struct SettingRow {
    spec: SettingSpec,
    value: SettingValue,
}

#[derive(Debug)]
enum Command {
    Set { key: String, value: SettingValue },
    Reset { key: String },
    System(SystemCommand),
}

/// One change to a hardware level, or a plain re-read.
///
/// `Adjust` rather than read-modify-write for a step, because the service knows where the level
/// was and two clients on the same volume key must not each undo the other (`Audio1`'s own
/// reasoning). Dragging the track is the one case that genuinely means "put it *there*", and that
/// is what `Set` is for.
#[derive(Clone, Copy, Debug)]
enum SystemCommand {
    Refresh,
    SetVolume(u32),
    StepVolume(i32),
    ToggleMute,
    SetBrightness(u32),
    StepBrightness(i32),
}

#[derive(Debug)]
enum ResultMessage {
    Setting {
        key: String,
        result: Result<(), String>,
    },
    /// Every system command answers with the state that followed it, so the page shows what the
    /// service actually did rather than what was asked for — a level the service clamped is the
    /// normal case, not an edge one.
    System {
        state: Box<crate::system::SystemState>,
        error: Option<String>,
    },
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
                    let message = match command {
                        Command::System(system) => runtime.block_on(run_system_command(system)),
                        Command::Set { .. } | Command::Reset { .. } => {
                            runtime.block_on(run_setting_command(command))
                        }
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

/// One `Settings1` write, as a result the page can show.
async fn run_setting_command(command: Command) -> ResultMessage {
    let key = match &command {
        Command::Set { key, .. } | Command::Reset { key } => key.clone(),
        Command::System(_) => unreachable!("dispatched by the caller"),
    };
    let result = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = SettingsProxy::new(&connection).await?;
        match command {
            Command::Set { key, value } => {
                let json = serde_json::to_string(&value)?;
                proxy.set(&key, &json, "lege-settings-gui").await?;
            }
            Command::Reset { key } => proxy.reset(&key, "lege-settings-gui").await?,
            Command::System(_) => unreachable!("dispatched by the caller"),
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    ResultMessage::Setting {
        key,
        result: result.map_err(|error| format!("{error:#}")),
    }
}

/// One hardware change, followed by a re-read: the page reports what the service did, not what it
/// was asked to do. A failure is reported *and* followed by the re-read, so a refused write leaves
/// the controls showing the level that is really set rather than the one that was attempted.
async fn run_system_command(command: SystemCommand) -> ResultMessage {
    use crate::system::{self, SystemControl};
    let outcome = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let bus = system::Bus::connect(&connection).await?;
        let attempted = match command {
            SystemCommand::Refresh => Ok(()),
            SystemCommand::SetVolume(percent) => bus.set_volume(percent).await,
            SystemCommand::StepVolume(delta) => bus.adjust_volume(delta).await,
            SystemCommand::ToggleMute => bus.toggle_muted().await,
            SystemCommand::SetBrightness(percent) => bus.set_brightness(percent).await,
            SystemCommand::StepBrightness(delta) => bus.adjust_brightness(delta).await,
        };
        Ok::<_, anyhow::Error>((system::read(&bus).await, attempted))
    }
    .await;
    match outcome {
        Ok((state, attempted)) => ResultMessage::System {
            state: Box::new(state),
            error: attempted.err().map(|error| format!("{error:#}")),
        },
        Err(error) => ResultMessage::System {
            state: Box::new(crate::system::SystemState::default()),
            error: Some(format!("{error:#}")),
        },
    }
}

struct SettingsApp {
    home: Vec<SettingRow>,
    system: crate::system::SystemState,
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
            match message {
                ResultMessage::Setting { key, result } => {
                    self.status = match result {
                        Ok(()) => format!("Saved {key}"),
                        Err(error) => format!("Could not save {key} — {error}"),
                    };
                }
                ResultMessage::System { state, error } => {
                    self.system = *state;
                    if let Some(error) = error {
                        self.status = format!("Could not change it — {error}");
                    }
                }
            }
        }
    }

    fn system(&mut self, command: SystemCommand) {
        let _ = self.backend.commands.send(Command::System(command));
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

    /// The System page: the battery, the volume and the display brightness.
    ///
    /// Same row shape as the Home form — a name, the sentence under it, and the controls on the
    /// right — because they are the same kind of thing and a person should not have to learn a
    /// second layout to change one of them.
    fn draw_system(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let battery = crate::system::battery_line(self.system.battery.as_ref());
        // Battery first and read-only: there is nothing to do to a battery from a settings page,
        // and a control here would have to be either a lie or a power profile (`Power1`).
        let _ = system_row(ui, area, 0, "Battery", &battery);

        let volume_line = crate::system::volume_line(self.system.audio.as_ref());
        let settable = crate::system::volume_settable(&self.system);
        let (percent, muted) = self
            .system
            .audio
            .as_ref()
            .map_or((0, false), |audio| (audio.volume_percent, audio.muted));
        let controls = system_row(ui, area, 1, "Volume", &volume_line);
        match level_controls(ui, controls, percent, settable, Some(muted)) {
            Some(LevelChange::Step(delta)) => self.system(SystemCommand::StepVolume(delta)),
            Some(LevelChange::ToggleMute) => self.system(SystemCommand::ToggleMute),
            // Only when the drag has actually moved it: `pressing` is true every frame the button
            // is held, and without this the page would write once per frame for as long as a
            // person rested their finger on the track.
            Some(LevelChange::Set(wanted)) if wanted != percent => {
                self.system(SystemCommand::SetVolume(wanted));
            }
            None | Some(LevelChange::Set(_)) => {}
        }

        let brightness_line = crate::system::brightness_line(self.system.backlight.as_ref());
        let settable = crate::system::brightness_settable(&self.system);
        let percent = self
            .system
            .backlight
            .as_ref()
            .map_or(0, |backlight| backlight.percent);
        let controls = system_row(ui, area, 2, "Brightness", &brightness_line);
        match level_controls(ui, controls, percent, settable, None) {
            Some(LevelChange::Step(delta)) => self.system(SystemCommand::StepBrightness(delta)),
            Some(LevelChange::Set(wanted)) if wanted != percent => {
                self.system(SystemCommand::SetBrightness(wanted));
            }
            // A display has no mute, so `level_controls` is never asked for one here.
            None | Some(LevelChange::ToggleMute | LevelChange::Set(_)) => {}
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

/// What a frame's input asked of one level's controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LevelChange {
    Step(i32),
    Set(u32),
    ToggleMute,
}

/// One row of the System page: name, the sentence under it, and the rectangle its controls get.
///
/// Returns the control area even for a read-only row; the caller simply does not draw into it.
fn system_row(ui: &mut Ui<'_>, area: Rect, index: i32, name: &str, detail: &str) -> Rect {
    let row = Rect::new(
        area.x,
        area.y + ui.px(88) * index,
        area.w,
        ui.px(78).min(area.h),
    );
    ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
    ui.painter
        .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);
    let controls_width = ui.px(360);
    let text_width = (row.w - controls_width - ui.px(24)).max(ui.px(80));
    ui.label(
        Rect::new(row.x + ui.px(12), row.y + ui.px(7), text_width, ui.px(24)),
        name,
        ui.theme.text,
        Align::Left,
    );
    ui.label(
        Rect::new(row.x + ui.px(12), row.y + ui.px(33), text_width, ui.px(34)),
        detail,
        ui.theme.text_dim,
        Align::Left,
    );
    Rect::new(
        row.right() - controls_width,
        row.y + ui.px(20),
        controls_width - ui.px(12),
        ui.px(38),
    )
}

/// The controls for one hardware level: a stepper either side of a track that can be clicked or
/// dragged to a value, plus Mute when `muted` is given.
///
/// Inert when `settable` is false — the track and the steppers are still drawn, dimmed, because a
/// control that vanishes leaves a person hunting for it, while a live one over a service that
/// cannot act is a promise the page cannot keep. The row's own sentence says why.
fn level_controls(
    ui: &mut Ui<'_>,
    area: Rect,
    percent: u32,
    settable: bool,
    muted: Option<bool>,
) -> Option<LevelChange> {
    if area.w <= 0 || area.h <= 0 {
        return None;
    }
    let button = ui.px(38);
    let gap = ui.px(8);
    let mute_width = if muted.is_some() { ui.px(78) + gap } else { 0 };
    let minus = Rect::new(area.x, area.y, button, area.h);
    let plus = Rect::new(area.right() - mute_width - button, area.y, button, area.h);
    let left = minus.right() + gap;
    let track = Rect::new(
        left,
        area.y + ui.px(22),
        (plus.x - gap - left).max(ui.px(20)),
        ui.px(8),
    );
    // The whole strip between the steppers takes the drag, not the eight-pixel bar: a target a
    // person has to hit exactly is a target they miss.
    let grip = Rect::new(track.x, area.y, track.w, area.h);

    ui.label(
        Rect::new(track.x, area.y, track.w, ui.px(18)),
        &format!("{percent}%"),
        if settable {
            ui.theme.text
        } else {
            ui.theme.text_dim
        },
        Align::Centre,
    );
    let radius = ui.px(4);
    ui.painter
        .fill_rounded_rect(track, radius, ui.theme.panel_edge);
    let filled = track.w * i32::try_from(percent.min(100)).unwrap_or(0) / 100;
    if filled > 0 {
        ui.painter.fill_rounded_rect(
            Rect::new(track.x, track.y, filled, track.h),
            radius,
            if settable {
                ui.theme.accent
            } else {
                ui.theme.text_dim
            },
        );
    }

    if !settable {
        for (rect, glyph) in [(minus, "−"), (plus, "+")] {
            ui.label(rect, glyph, ui.theme.text_dim, Align::Centre);
        }
        if let Some(muted) = muted {
            let rect = Rect::new(plus.right() + gap, area.y, ui.px(78), area.h);
            ui.label(
                rect,
                if muted { "Unmute" } else { "Mute" },
                ui.theme.text_dim,
                Align::Centre,
            );
        }
        return None;
    }

    let mut change = None;
    if ui.button(minus, "−") {
        change = Some(LevelChange::Step(-STEP_PERCENT));
    }
    if ui.button(plus, "+") {
        change = Some(LevelChange::Step(STEP_PERCENT));
    }
    if let Some(muted) = muted {
        let rect = Rect::new(plus.right() + gap, area.y, ui.px(78), area.h);
        if ui.button(rect, if muted { "Unmute" } else { "Mute" }) {
            change = Some(LevelChange::ToggleMute);
        }
    }
    if change.is_none()
        && ui.input.pressing(grip)
        && let Some((x, _)) = ui.input.cursor()
    {
        let wanted = ((x - track.x) * 100 / track.w.max(1)).clamp(0, 100);
        change = Some(LevelChange::Set(u32::try_from(wanted).unwrap_or(0)));
    }
    change
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
            if ui.tabs(&mut self.selected_page, tabs, &PAGE_NAMES)
                && self.selected_page == SYSTEM_PAGE
            {
                // Nothing pushes a level change at this window, so arriving on the page is when
                // it re-reads. A stale charge is the one reading that goes wrong on its own.
                self.system(SystemCommand::Refresh);
            }
            let content = Rect::new(
                inset.x,
                tabs.bottom() + ui.px(14),
                inset.w,
                inset.bottom() - tabs.bottom() - ui.px(52),
            );
            match self.selected_page {
                HOME_PAGE => self.draw_home(&mut ui, content),
                SYSTEM_PAGE => self.draw_system(&mut ui, content),
                _ => self.draw_text_page(&mut ui, content),
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

    // One entry per tab, so `selected_page` indexes both. The two interactive pages draw their own
    // controls and keep an empty entry here rather than a rendered copy of themselves.
    let pages = vec![
        Vec::new(),
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
    // The System page's first reading, taken here with the rest so the page is populated the
    // moment it is opened rather than blank until the first command comes back. Every interface
    // it reads degrades on its own (`system::read`), so a machine with none of them still loads.
    let system = match crate::system::Bus::connect(&connection).await {
        Ok(bus) => crate::system::read(&bus).await,
        Err(_) => crate::system::SystemState::default(),
    };

    Ok(SettingsApp {
        home,
        system,
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
