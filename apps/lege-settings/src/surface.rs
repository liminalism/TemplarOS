//! Graphical Settings surface. The Home Field form is generated from `Settings1.SchemaDetails`;
//! bounds and choices therefore remain service policy rather than becoming UI constants.

use crate::background::BackgroundEntry;
use crate::history::Event;
use crate::providers::{ChoiceOption, RoleChoice};
use anyhow::{Context, Result};
use lege_client::proxies::SettingsProxy;
use lege_session_core::settings::{SettingSpec, SettingValue};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{
    Input, KeyInput, Modifiers, MouseButton, PixelApp, Scale, Waker, WindowConfig, run_app,
};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{Dropdown, ScrollState, Theme, Ui};
use std::sync::{Arc, Mutex, mpsc};

const HOME_PREFIX: &str = "shell.home-field.";
/// `System` is second because it is the page the Home Field's Settings widget opens into: the
/// widget reports the battery, the volume and the brightness, and this is where they can be set
/// outright. The rest keep the order they have had, so a person who learned where a tab is does
/// not have to learn it again.
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
const PROVIDERS_PAGE: usize = 2;
const BACKGROUND_PAGE: usize = 4;
const HISTORY_PAGE: usize = 6;
/// How far one press of a stepper moves a level: the same five points `lege status louder` and
/// the Home Field's own controls use, so no two surfaces disagree about what one step means.
const STEP_PERCENT: i32 = 5;
/// How many recent events the History page holds. A settings page is read to find something that
/// happened today, not to audit a month, and every row is parsed and laid out on the UI thread.
const HISTORY_LIMIT: u32 = 200;

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
    Background(BackgroundCommand),
    History(HistoryCommand),
    Providers(ProvidersCommand),
}

/// One change to who fills a role, or a plain re-read.
#[derive(Clone, Debug)]
enum ProvidersCommand {
    Refresh,
    /// Pin `app_id` to `role`.
    Select {
        role: String,
        app_id: String,
    },
    /// Return `role` to choosing automatically.
    Deselect {
        role: String,
    },
}

/// One change to a background-activity approval, or a plain re-read.
#[derive(Clone, Debug)]
enum BackgroundCommand {
    Refresh,
    Grant(String),
    Revoke(String),
}

/// One undo, or a plain re-read.
#[derive(Clone, Debug)]
enum HistoryCommand {
    Refresh,
    Undo(String),
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
    /// Same discipline for the two list pages: a write is always followed by the re-read, so the
    /// rows show the state the service is actually in and never the one the click asked for.
    Background {
        entries: Vec<BackgroundEntry>,
        message: Option<String>,
    },
    History {
        events: Vec<Event>,
        message: Option<String>,
    },
    Providers {
        choices: Vec<RoleChoice>,
        message: Option<String>,
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
                        Command::Background(background) => {
                            runtime.block_on(run_background_command(background))
                        }
                        Command::History(history) => runtime.block_on(run_history_command(history)),
                        Command::Providers(providers) => {
                            runtime.block_on(run_providers_command(providers))
                        }
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
        Command::System(_)
        | Command::Background(_)
        | Command::History(_)
        | Command::Providers(_) => unreachable!("dispatched by the caller"),
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
            Command::System(_)
            | Command::Background(_)
            | Command::History(_)
            | Command::Providers(_) => unreachable!("dispatched by the caller"),
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

/// One approval change, followed by a re-read of the whole list.
///
/// The re-read is not an optimisation away from a local edit: `Grant`/`Revoke` write the same
/// `autostart_approvals` the planner consults, and granting one application can change what the
/// session reports about another. Only the service knows the result of that, so the page asks.
async fn run_background_command(command: BackgroundCommand) -> ResultMessage {
    use crate::background::{self, BackgroundSource};
    let outcome = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = background::connect(&connection).await?;
        // Through the trait, not the proxy's inherent method of the same name: the trait's
        // implementation is what names the call in the error a person ends up reading.
        let attempted = match &command {
            BackgroundCommand::Refresh => Ok(()),
            BackgroundCommand::Grant(app_id) => BackgroundSource::grant(&proxy, app_id).await,
            BackgroundCommand::Revoke(app_id) => BackgroundSource::revoke(&proxy, app_id).await,
        };
        Ok::<_, anyhow::Error>((background::entries_with(&proxy).await, attempted))
    }
    .await;
    match outcome {
        Ok((entries, attempted)) => ResultMessage::Background {
            entries: entries.clone().unwrap_or_default(),
            message: match (attempted, entries) {
                (Err(error), _) => Some(format!("Could not change it — {error:#}")),
                (Ok(()), Err(unavailable)) => Some(unavailable),
                (Ok(()), Ok(_)) => background_confirmation(&command),
            },
        },
        Err(error) => ResultMessage::Background {
            entries: Vec::new(),
            message: Some(format!("Could not reach Background1 — {error:#}")),
        },
    }
}

fn background_confirmation(command: &BackgroundCommand) -> Option<String> {
    match command {
        BackgroundCommand::Refresh => None,
        BackgroundCommand::Grant(app_id) => Some(format!("{app_id} may run in the background")),
        // Deliberately not "stopped": `Revoke` takes effect at the next autostart decision and
        // does not kill a running instance, and a page that implies otherwise is lying about
        // what the button did.
        BackgroundCommand::Revoke(app_id) => Some(format!(
            "{app_id} is blocked from starting in the background; a copy already running keeps running"
        )),
    }
}

/// One undo, followed by a re-read of recent history.
///
/// `Undo` answering `false` is not an error — it is the service saying the event was already
/// undone or was never reversible — and it is reported as the plain fact it is.
async fn run_history_command(command: HistoryCommand) -> ResultMessage {
    use crate::history::{self, HistorySource};
    let outcome = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = lege_client::proxies::HistoryProxy::new(&connection)
            .await
            .context("History1")?;
        let attempted = match &command {
            HistoryCommand::Refresh => Ok(None),
            HistoryCommand::Undo(event_id) => HistorySource::undo(&proxy, event_id).await.map(Some),
        };
        let events = HistorySource::recent(&proxy, HISTORY_LIMIT)
            .await
            .map(|raw| history::parse_events(&raw));
        Ok::<_, anyhow::Error>((events, attempted))
    }
    .await;
    match outcome {
        Ok((events, attempted)) => ResultMessage::History {
            events: events.as_ref().cloned().unwrap_or_default(),
            message: match (attempted, events) {
                (Err(error), _) => Some(format!("Could not undo it — {error:#}")),
                (Ok(_), Err(error)) => Some(format!("Could not read history — {error:#}")),
                (Ok(Some(true)), Ok(_)) => Some("Undone".to_owned()),
                (Ok(Some(false)), Ok(_)) => {
                    Some("Nothing to undo — it was already undone".to_owned())
                }
                (Ok(None), Ok(_)) => None,
            },
        },
        Err(error) => ResultMessage::History {
            events: Vec::new(),
            message: Some(format!("Could not reach History1 — {error:#}")),
        },
    }
}

/// One provider change, followed by a re-read of every role.
///
/// The whole report, not the one role: an exclusive role's resolution is a function of what every
/// application declares, and `Providers1.Changed` names one role while the *reasons* shown under
/// the others can move with it.
async fn run_providers_command(command: ProvidersCommand) -> ResultMessage {
    use crate::providers::{self, ProvidersSource};
    let outcome = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = lege_client::proxies::ProvidersProxy::new(&connection)
            .await
            .context("Providers1")?;
        let attempted = match &command {
            ProvidersCommand::Refresh => Ok(()),
            ProvidersCommand::Select { role, app_id } => {
                ProvidersSource::select(&proxy, role, app_id).await
            }
            ProvidersCommand::Deselect { role } => ProvidersSource::deselect(&proxy, role).await,
        };
        Ok::<_, anyhow::Error>((providers::role_choices_with(&proxy).await, attempted))
    }
    .await;
    match outcome {
        Ok((choices, attempted)) => ResultMessage::Providers {
            choices: choices.clone().unwrap_or_default(),
            message: match (attempted, choices) {
                (Err(error), _) => Some(format!("Could not change it — {error:#}")),
                (Ok(()), Err(unavailable)) => Some(unavailable),
                (Ok(()), Ok(_)) => providers_confirmation(&command),
            },
        },
        Err(error) => ResultMessage::Providers {
            choices: Vec::new(),
            message: Some(format!("Could not reach Providers1 — {error:#}")),
        },
    }
}

fn providers_confirmation(command: &ProvidersCommand) -> Option<String> {
    match command {
        ProvidersCommand::Refresh => None,
        ProvidersCommand::Select { role, app_id } => Some(format!("{app_id} now fills {role}")),
        ProvidersCommand::Deselect { role } => Some(format!("{role} chooses automatically again")),
    }
}

struct SettingsApp {
    home: Vec<SettingRow>,
    system: crate::system::SystemState,
    background: Vec<BackgroundEntry>,
    history: Vec<Event>,
    providers: Vec<RoleChoice>,
    /// The one open chooser, if any. Immediate mode has no z-order, so a dropdown's popup is
    /// drawn last, after every row — see `pixelkit_ui::Dropdown`.
    provider_dropdown: Dropdown,
    /// Which role's chooser is open, so the popup's pick is applied to the right row.
    provider_open_row: Option<usize>,
    pages: Vec<Vec<String>>,
    selected_page: usize,
    /// One scroll position per tab, so leaving a page and coming back does not lose the place a
    /// person had scrolled to — and so a long page can be read at all, which is the part that
    /// was missing while every page drew its first twenty lines and stopped.
    scroll: [ScrollState; PAGE_NAMES.len()],
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
                ResultMessage::Background { entries, message } => {
                    self.background = entries;
                    if let Some(message) = message {
                        self.status = message;
                    }
                }
                ResultMessage::History { events, message } => {
                    self.history = events;
                    if let Some(message) = message {
                        self.status = message;
                    }
                }
                ResultMessage::Providers { choices, message } => {
                    self.providers = choices;
                    if let Some(message) = message {
                        self.status = message;
                    }
                }
            }
        }
    }

    fn system(&mut self, command: SystemCommand) {
        let _ = self.backend.commands.send(Command::System(command));
    }

    /// Re-read whichever page was just opened. Nothing pushes at this window, so arriving on a
    /// page is when it refreshes; the two static pages have nothing to re-read.
    fn refresh_page(&mut self, page: usize) {
        let command = match page {
            SYSTEM_PAGE => Command::System(SystemCommand::Refresh),
            PROVIDERS_PAGE => Command::Providers(ProvidersCommand::Refresh),
            BACKGROUND_PAGE => Command::Background(BackgroundCommand::Refresh),
            HISTORY_PAGE => Command::History(HistoryCommand::Refresh),
            _ => return,
        };
        let _ = self.backend.commands.send(command);
    }

    fn draw_home(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let rows = self.home.clone();
        let heights = vec![ui.px(88); rows.len()];
        let mut wanted: Option<(usize, Option<SettingValue>)> = None;
        let mut scroll = self.scroll[HOME_PAGE];
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let row_area = Rect::new(rect.x, rect.y, rect.w, ui.px(78));
            ui.painter
                .fill_rounded_rect(row_area, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row_area, ui.scale.hairline(), ui.theme.panel_edge);
            let name = rows[index]
                .spec
                .key
                .strip_prefix(HOME_PREFIX)
                .unwrap_or(&rows[index].spec.key)
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
                &rows[index].spec.description,
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
                wanted = Some((index, None));
            }
            if let Some(value) = home_control(ui, &rows[index], controls) {
                wanted = Some((index, Some(value)));
            }
        });
        self.scroll[HOME_PAGE] = scroll;
        match wanted {
            Some((index, Some(value))) => self.change(index, value),
            Some((index, None)) => self.reset(index),
            None => {}
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

    /// The Providers page: which application fills each exclusive role, why, and a chooser to
    /// change it.
    ///
    /// The sentence stays. A chooser without it would show *what* is selected and never *why* —
    /// and "why" here is the difference between a role a person pinned and one that resolved by
    /// priority, which look identical in a dropdown and behave differently when something new is
    /// installed.
    fn draw_providers(&mut self, ui: &mut Ui<'_>, area: Rect) {
        if self.providers.is_empty() {
            ui.label(
                Rect::new(area.x, area.y, area.w, ui.px(24)),
                "Providers1 is not answering.",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let choices = self.providers.clone();
        let heights = vec![ui.px(84); choices.len()];
        let mut opened: Option<usize> = None;
        let mut scroll = self.scroll[PROVIDERS_PAGE];
        let dropdown = &mut self.provider_dropdown;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let choice = &choices[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(74));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            let field_w = ui.px(300);
            let text_w = (row.w - field_w - ui.px(36)).max(ui.px(120));
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(8), text_w, ui.px(22)),
                &choice.role.to_string(),
                ui.theme.text,
                Align::Left,
            );
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(32), text_w, ui.px(34)),
                &choice.explanation,
                ui.theme.text_dim,
                Align::Left,
            );

            let labels: Vec<String> = choice.options.iter().map(ChoiceOption::label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let field = Rect::new(
                row.right() - field_w - ui.px(12),
                row.y + ui.px(20),
                field_w,
                ui.px(34),
            );
            if ui.dropdown(dropdown, field, &refs, choice.selected) {
                opened = Some(index);
            }
        });
        self.scroll[PROVIDERS_PAGE] = scroll;
        if let Some(index) = opened {
            self.provider_open_row = self.provider_dropdown.is_open().then_some(index);
        }
    }

    /// The open chooser's popup, drawn after every page so nothing covers it, and the pick it
    /// reports applied to the row that opened it.
    fn draw_provider_popup(&mut self, ui: &mut Ui<'_>, bounds: Rect) {
        let mut dropdown = std::mem::take(&mut self.provider_dropdown);
        let picked = dropdown.draw(ui, bounds);
        self.provider_dropdown = dropdown;
        let (Some(picked), Some(row)) = (picked, self.provider_open_row) else {
            return;
        };
        self.provider_open_row = None;
        let Some(choice) = self.providers.get(row) else {
            return;
        };
        let Some(option) = choice.options.get(picked) else {
            return;
        };
        let role = choice.role.to_string();
        let command = match option {
            ChoiceOption::Automatic => ProvidersCommand::Deselect { role },
            ChoiceOption::App { app_id, .. } => ProvidersCommand::Select {
                role,
                app_id: app_id.clone(),
            },
        };
        "Saving…".clone_into(&mut self.status);
        let _ = self.backend.commands.send(Command::Providers(command));
    }

    /// The Background page: every application that declares background activity, what it asked
    /// for, whether it is running and why, and the one decision a person makes about it.
    ///
    /// Allow and Block are two buttons rather than one toggle because the service has three
    /// states, not two: an application that has never been answered is *undecided*, which a
    /// toggle would have to draw as one of the other two and thereby claim a decision nobody
    /// made. The current state is the badge; the buttons say what a click would do.
    fn draw_background(&mut self, ui: &mut Ui<'_>, area: Rect) {
        if self.background.is_empty() {
            ui.label(
                Rect::new(area.x, area.y, area.w, ui.px(24)),
                "No application declares background activity.",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let entries = self.background.clone();
        let heights = vec![ui.px(96); entries.len()];
        let mut wanted: Option<BackgroundCommand> = None;
        let mut scroll = self.scroll[BACKGROUND_PAGE];
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let entry = &entries[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(86));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            let buttons = ui.px(168);
            let text_w = (row.w - buttons - ui.px(24)).max(ui.px(120));
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(6), text_w, ui.px(24)),
                &entry.app_id,
                ui.theme.text,
                Align::Left,
            );
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(30), text_w, ui.px(22)),
                &crate::background::permissions_line(entry),
                ui.theme.text_dim,
                Align::Left,
            );
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(52), text_w, ui.px(22)),
                &crate::background::running_line(entry),
                ui.theme.text_dim,
                Align::Left,
            );

            let (badge_text, badge_fill) = approval_colours(ui.theme, entry.approval);
            ui.badge(
                Rect::new(
                    row.right() - buttons - ui.px(6),
                    row.y + ui.px(8),
                    ui.px(96),
                    ui.px(22),
                ),
                entry.approval.label(),
                badge_text,
                badge_fill,
                ui.theme.panel_edge,
            );

            let allow = Rect::new(
                row.right() - buttons,
                row.y + ui.px(40),
                ui.px(76),
                ui.px(32),
            );
            let block = Rect::new(allow.right() + ui.px(8), allow.y, ui.px(76), allow.h);
            if ui.button(allow, "Allow") {
                wanted = Some(BackgroundCommand::Grant(entry.app_id.clone()));
            }
            if ui.button(block, "Block") {
                wanted = Some(BackgroundCommand::Revoke(entry.app_id.clone()));
            }
        });
        self.scroll[BACKGROUND_PAGE] = scroll;
        if let Some(command) = wanted {
            "Saving…".clone_into(&mut self.status);
            let _ = self.backend.commands.send(Command::Background(command));
        }
    }

    /// The History page: what the session decided, when, why — and Undo where the service says
    /// the event can still be reversed.
    ///
    /// Only reversible events get a button. `History1.Undo` on an irreversible event answers
    /// "nothing to undo" rather than failing, so a button on every row would work; it would also
    /// be a row of buttons that mostly do nothing, which is worse than no button at all.
    fn draw_history(&mut self, ui: &mut Ui<'_>, area: Rect) {
        if self.history.is_empty() {
            ui.label(
                Rect::new(area.x, area.y, area.w, ui.px(24)),
                "Nothing has been recorded yet.",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let events = self.history.clone();
        let now = crate::history::now_ms();
        let heights = vec![ui.px(62); events.len()];
        let mut wanted: Option<String> = None;
        let mut scroll = self.scroll[HISTORY_PAGE];
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let event = &events[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(54));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            let kind_w = ui.px(132);
            let undo_w = if event.reversible { ui.px(84) } else { 0 };
            let text_x = row.x + kind_w + ui.px(16);
            let text_w = (row.right() - text_x - undo_w - ui.px(16)).max(ui.px(80));
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(6), kind_w, ui.px(20)),
                &event.kind.to_string().replace('-', " "),
                ui.theme.accent,
                Align::Left,
            );
            ui.label(
                Rect::new(text_x, row.y + ui.px(6), text_w, ui.px(22)),
                event.headline(),
                ui.theme.text,
                Align::Left,
            );
            ui.label(
                Rect::new(text_x, row.y + ui.px(28), text_w, ui.px(20)),
                &event.detail(now),
                ui.theme.text_dim,
                Align::Left,
            );
            if event.reversible {
                let undo = Rect::new(
                    row.right() - undo_w - ui.px(8),
                    row.y + ui.px(11),
                    ui.px(76),
                    ui.px(32),
                );
                if ui.button(undo, "Undo") {
                    wanted = Some(event.id.clone());
                }
            }
        });
        self.scroll[HISTORY_PAGE] = scroll;
        if let Some(event_id) = wanted {
            "Undoing…".clone_into(&mut self.status);
            let _ = self
                .backend
                .commands
                .send(Command::History(HistoryCommand::Undo(event_id)));
        }
    }

    /// A page whose content is text the page module rendered: scrolled, not truncated.
    ///
    /// A rendered page is one paragraph per subject with its detail indented under it, so a line
    /// that starts with whitespace continues the one above and is drawn dimmer.
    fn draw_text_page(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let lines = self
            .pages
            .get(self.selected_page)
            .cloned()
            .unwrap_or_default();
        let line_height = ui.px(26);
        let heights = vec![line_height; lines.len()];
        let mut scroll = self.scroll[self.selected_page];
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let line = &lines[index];
            let continuation = line.starts_with(char::is_whitespace);
            ui.label(
                Rect::new(rect.x, rect.y, rect.w, rect.h),
                line.trim_start(),
                if continuation {
                    ui.theme.text_dim
                } else {
                    ui.theme.text
                },
                Align::Left,
            );
        });
        self.scroll[self.selected_page] = scroll;
    }
}

/// The badge colours for one approval state.
///
/// Blocked is the only one drawn in the theme's plain text colour on the panel edge: a red badge
/// would read as a fault, and a person deliberately blocking an application has not caused one.
fn approval_colours(theme: Theme, approval: crate::background::Approval) -> (u32, u32) {
    use crate::background::Approval;
    match approval {
        Approval::Approved => (theme.accent, theme.accent_soft),
        Approval::Denied => (theme.text_dim, theme.panel_edge),
        Approval::Undecided => (theme.text, theme.hover),
    }
}

/// The editing control for one Home setting, chosen by the value's type: a toggle for a flag, a
/// stepper clamped to the spec's bounds for a number, and a cycling button for a choice.
///
/// A free function rather than a method because it is drawn from inside a scroll list's row
/// closure, which already holds the page mutably; it reports the value a person asked for and
/// the page applies it once the list is done.
fn home_control(ui: &mut Ui<'_>, row: &SettingRow, controls: Rect) -> Option<SettingValue> {
    match row.value.clone() {
        SettingValue::Bool(mut enabled) => {
            let label = if enabled { "On" } else { "Off" };
            ui.toggle(controls, &mut enabled, label)
                .then_some(SettingValue::Bool(enabled))
        }
        SettingValue::Int(value) => {
            let minus = Rect::new(controls.x, controls.y, ui.px(38), controls.h);
            let plus = Rect::new(
                controls.right() - ui.px(38),
                controls.y,
                ui.px(38),
                controls.h,
            );
            let mut wanted = None;
            if ui.button(minus, "−") {
                let min = row.spec.min.unwrap_or(i64::MIN);
                wanted = Some(SettingValue::Int(value.saturating_sub(1).max(min)));
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
                let max = row.spec.max.unwrap_or(i64::MAX);
                wanted = Some(SettingValue::Int(value.saturating_add(1).min(max)));
            }
            wanted
        }
        SettingValue::Choice(value) => {
            let choices = &row.spec.choices;
            if ui.button(controls, &value) && !choices.is_empty() {
                let position = choices
                    .iter()
                    .position(|choice| choice == &value)
                    .unwrap_or(0);
                Some(SettingValue::Choice(
                    choices[(position + 1) % choices.len()].clone(),
                ))
            } else {
                None
            }
        }
        other => {
            ui.label(
                controls,
                &format!("{other:?}"),
                ui.theme.text,
                Align::Centre,
            );
            None
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
            if ui.tabs(&mut self.selected_page, tabs, &PAGE_NAMES) {
                // Nothing pushes at this window, so arriving on a page is when it re-reads. A
                // stale charge, a stale approval and a stale history are each the kind of
                // reading that goes wrong on its own between visits.
                self.refresh_page(self.selected_page);
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
                PROVIDERS_PAGE => self.draw_providers(&mut ui, content),
                BACKGROUND_PAGE => self.draw_background(&mut ui, content),
                HISTORY_PAGE => self.draw_history(&mut ui, content),
                _ => self.draw_text_page(&mut ui, content),
            }
            // Last, so nothing the page drew afterwards covers the open list.
            self.draw_provider_popup(&mut ui, content);
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

    // One entry per tab, so `selected_page` indexes both. The four interactive pages draw their
    // own controls and keep an empty entry here rather than a rendered copy of themselves.
    let pages = vec![
        Vec::new(),
        Vec::new(),
        Vec::new(),
        super::associations::render(&connection, None, "")
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        Vec::new(),
        super::resources::render(&connection, 20)
            .await
            .lines()
            .map(str::to_owned)
            .collect(),
        Vec::new(),
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

    // Same reasoning for the two list pages: populated at startup, refreshed on every visit.
    // A service that is not answering yields an empty page, never a failed launch — Settings is
    // where a person goes *because* something is wrong.
    let background = match crate::background::connect(&connection).await {
        Ok(proxy) => crate::background::entries_with(&proxy)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let providers = match lege_client::proxies::ProvidersProxy::new(&connection).await {
        Ok(proxy) => crate::providers::role_choices_with(&proxy)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let history = match lege_client::proxies::HistoryProxy::new(&connection).await {
        Ok(proxy) => proxy
            .recent(HISTORY_LIMIT)
            .await
            .map(|raw| crate::history::parse_events(&raw))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    Ok(SettingsApp {
        home,
        system,
        background,
        history,
        providers,
        provider_dropdown: Dropdown::new(),
        provider_open_row: None,
        pages,
        selected_page: 0,
        scroll: [ScrollState::new(); PAGE_NAMES.len()],
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
