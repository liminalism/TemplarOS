//! Graphical Settings surface. The Home Field form is generated from `Settings1.SchemaDetails`;
//! bounds and choices therefore remain service policy rather than becoming UI constants.

use crate::associations::MimeChoice;
use crate::background::BackgroundEntry;
use crate::history::Event;
use crate::names::AppNames;
use crate::providers::{ChoiceOption, RoleChoice};
use anyhow::{Context, Result};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, Modifiers, MouseButton, Scale};
use pixelkit_text::{Align, Face, FontSet, TextCache};
use pixelkit_ui::{Dropdown, ScrollState, Theme, Ui};
use pixelkit_windowing::{PixelApp, Waker, WindowConfig, run_app};
use std::sync::{Arc, Mutex, mpsc};
use templar_client::proxies::SettingsProxy;
use templar_session_core::settings::{SettingSpec, SettingValue};

const HOME_PREFIX: &str = "shell.home-field.";
const APPEARANCE_PREFIX: &str = "shell.appearance.";
const GARDEN_PREFIX: &str = "shell.home-garden.";
/// The one `shell.home-garden.*` key the typed key form cannot draw: a list of directory names
/// does not fit a 184-pixel control rectangle, so the Homegarden page gives it a section.
const GARDEN_IGNORE_KEY: &str = "shell.home-garden.ignore";
const RAILS_PREFIX: &str = "shell.rails.";
/// The Rails page's list key, given a section for the same reason as `GARDEN_IGNORE_KEY`.
const RAILS_EXCEPTIONS_KEY: &str = "shell.rails.exceptions";
/// `System` is second because it is the page the Home Field's Settings widget opens into: the
/// widget reports the battery, the volume and the brightness, and this is where they can be set
/// outright. The rest keep the order they have had, so a person who learned where a tab is does
/// not have to learn it again.
/// Appearance is last rather than beside Home Field for the same reason: it is new, and moving
/// six tabs to make room for it would cost every person who already knows where History is.
/// Homegarden is appended rather than placed beside Home Field for the same reason Appearance
/// was: it is new, and moving nine tabs to make room for it would cost every person who already
/// knows where History is. Rails is appended for the same reason again.
const PAGE_NAMES: [&str; 11] = [
    "Home Field",
    "System",
    "Providers",
    "Associations",
    "Background",
    "Resources",
    "History",
    "Hardware",
    "Appearance",
    "Homegarden",
    "Rails",
];
/// The index of each page that draws controls rather than rendered text.
const HOME_PAGE: usize = 0;
const SYSTEM_PAGE: usize = 1;
const PROVIDERS_PAGE: usize = 2;
const ASSOCIATIONS_PAGE: usize = 3;
const BACKGROUND_PAGE: usize = 4;
const HISTORY_PAGE: usize = 6;
const APPEARANCE_PAGE: usize = 8;
const GARDEN_PAGE: usize = 9;
const RAILS_PAGE: usize = 10;
/// How far one press of a stepper moves a level: the same five points `templar status louder` and
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
    Associations(AssociationsCommand),
    Garden(GardenCommand),
}

/// One change to Home's garden that is not a typed setting, or a plain re-read.
#[derive(Clone, Debug)]
enum GardenCommand {
    Refresh,
    /// Put one hidden place back on Home (`HomeGarden1.Hide(path, false)`).
    Unhide(String),
}

/// One change to which application opens a MIME type, or a plain re-read.
#[derive(Clone, Debug)]
enum AssociationsCommand {
    Refresh,
    SetMimeDefault {
        mime: String,
        app_id: String,
    },
    /// Remove the stored default, so the type is resolved or asked about again.
    ForgetMime {
        mime: String,
    },
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
    /// How long the session may be idle before the screen turns off, in seconds; 0 is never.
    /// `Power1` policy, enforced by the compositor (`compositor/templar-comp/src/idle.rs`).
    SetScreenOff(u32),
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
    Associations {
        choices: Vec<MimeChoice>,
        message: Option<String>,
    },
    Garden {
        hidden: Vec<String>,
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
            .name("templar-settings-writes".into())
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
                        Command::Associations(associations) => {
                            runtime.block_on(run_associations_command(associations))
                        }
                        Command::Garden(garden) => runtime.block_on(run_garden_command(garden)),
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

/// One change to Home's garden, then a re-read: `Contexts` is the only account of what is hidden,
/// so a page that unhid a place without re-reading would keep offering to unhide it.
async fn run_garden_command(command: GardenCommand) -> ResultMessage {
    let message = match run_garden(&command).await {
        Ok(message) => message,
        Err(error) => Some(format!("Could not reach the garden — {error}")),
    };
    let hidden = match zbus::Connection::session().await {
        Ok(connection) => crate::garden::fetch_hidden(&connection)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    ResultMessage::Garden { hidden, message }
}

async fn run_garden(command: &GardenCommand) -> Result<Option<String>> {
    let connection = zbus::Connection::session().await.context("session bus")?;
    match command {
        GardenCommand::Refresh => Ok(None),
        GardenCommand::Unhide(path) => {
            crate::garden::unhide(&connection, path).await?;
            Ok(Some(format!("{path} can grow on Home again")))
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
        | Command::Providers(_)
        | Command::Associations(_)
        | Command::Garden(_) => unreachable!("dispatched by the caller"),
    };
    let result = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = SettingsProxy::new(&connection).await?;
        match command {
            Command::Set { key, value } => {
                let json = serde_json::to_string(&value)?;
                proxy.set(&key, &json, "templar-settings-gui").await?;
            }
            Command::Reset { key } => proxy.reset(&key, "templar-settings-gui").await?,
            Command::System(_)
            | Command::Background(_)
            | Command::History(_)
            | Command::Providers(_)
            | Command::Associations(_)
            | Command::Garden(_) => unreachable!("dispatched by the caller"),
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
    use crate::system::{self, SystemControl, SystemSource};
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
            SystemCommand::SetScreenOff(seconds) => {
                // Which source's profile to write is a reading of the machine, not a choice on
                // the page — writing "on battery" while the laptop is plugged in would be a
                // control that appears to do nothing.
                let battery = bus.battery().await.ok().and_then(|json| {
                    serde_json::from_str::<templar_client::status::BatteryState>(&json).ok()
                });
                let source = system::source_name(system::power_source_for(battery.as_ref()));
                bus.set_screen_off_after(source, seconds).await
            }
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
        let proxy = templar_client::proxies::HistoryProxy::new(&connection)
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
        let proxy = templar_client::proxies::ProvidersProxy::new(&connection)
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

/// One default change, followed by a re-read of every type that has one.
///
/// `Forget` on a type whose default was already gone answers `false` rather than failing, which
/// is the plain fact and is reported as such — the chooser was already where the click asked it
/// to be.
async fn run_associations_command(command: AssociationsCommand) -> ResultMessage {
    use crate::associations::{self, AssociationsSource};
    let outcome = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let proxy = templar_client::proxies::AssociationsProxy::new(&connection)
            .await
            .context("Associations1")?;
        let attempted = match &command {
            AssociationsCommand::Refresh => Ok(()),
            AssociationsCommand::SetMimeDefault { mime, app_id } => {
                AssociationsSource::set_mime_default(&proxy, mime, app_id).await
            }
            AssociationsCommand::ForgetMime { mime } => {
                AssociationsSource::forget(&proxy, "mime", mime, "")
                    .await
                    .map(|_| ())
            }
        };
        Ok::<_, anyhow::Error>((
            associations::mime_choices_with(&proxy, true).await,
            attempted,
        ))
    }
    .await;
    match outcome {
        Ok((choices, attempted)) => ResultMessage::Associations {
            choices: choices.clone().unwrap_or_default(),
            message: match (attempted, choices) {
                (Err(error), _) => Some(format!("Could not change it — {error:#}")),
                (Ok(()), Err(unavailable)) => Some(unavailable),
                (Ok(()), Ok(_)) => associations_confirmation(&command),
            },
        },
        Err(error) => ResultMessage::Associations {
            choices: Vec::new(),
            message: Some(format!("Could not reach Associations1 — {error:#}")),
        },
    }
}

fn associations_confirmation(command: &AssociationsCommand) -> Option<String> {
    match command {
        AssociationsCommand::Refresh => None,
        AssociationsCommand::SetMimeDefault { mime, app_id } => {
            Some(format!("{mime} now opens with {app_id}"))
        }
        AssociationsCommand::ForgetMime { mime } => {
            Some(format!("{mime} has no default; you will be asked"))
        }
    }
}

struct SettingsApp {
    home: Vec<SettingRow>,
    /// `shell.appearance.*`, kept apart from the Home Field's keys so each page lists only its
    /// own — a page that shows a key it is not about is a page a person has to read twice.
    appearance: Vec<SettingRow>,
    /// `shell.home-garden.*`, same rule.
    garden: Vec<SettingRow>,
    /// `shell.rails.*`, same rule.
    rails: Vec<SettingRow>,
    /// The places `HomeGarden1` is holding hidden, so a person can put one back.
    hidden_places: Vec<String>,
    /// What is typed into the Homegarden page's "add an ignored directory" field. Each text field
    /// lives on its own page and only one page is on screen, so the one drawn is always the thing
    /// with focus and there is no focus state to keep.
    ignore_draft: pixelkit_ui::TextFieldState,
    /// What is typed into the Rails page's "add an exception" field; same rule.
    exception_draft: pixelkit_ui::TextFieldState,
    system: crate::system::SystemState,
    background: Vec<BackgroundEntry>,
    history: Vec<Event>,
    providers: Vec<RoleChoice>,
    associations: Vec<MimeChoice>,
    /// Display names for every installed application, read once at startup. Ids are what the
    /// services are told; names are what the pages show.
    names: AppNames,
    /// The one open chooser, if any. Immediate mode has no z-order, so a dropdown's popup is
    /// drawn last, after every row — see `pixelkit_ui::Dropdown`. One is enough for the whole
    /// window: only one page is on screen at a time, and switching tabs closes it.
    chooser: Dropdown,
    /// Which row on the current page opened it, so the popup's pick reaches the right one.
    chooser_row: Option<usize>,
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
    /// The rows one page of typed keys is editing.
    fn rows_for(&self, page: usize) -> &[SettingRow] {
        match page {
            APPEARANCE_PAGE => &self.appearance,
            GARDEN_PAGE => &self.garden,
            RAILS_PAGE => &self.rails,
            _ => &self.home,
        }
    }

    fn rows_for_mut(&mut self, page: usize) -> &mut Vec<SettingRow> {
        match page {
            APPEARANCE_PAGE => &mut self.appearance,
            GARDEN_PAGE => &mut self.garden,
            RAILS_PAGE => &mut self.rails,
            _ => &mut self.home,
        }
    }

    fn change(&mut self, page: usize, index: usize, value: SettingValue) {
        let rows = self.rows_for_mut(page);
        rows[index].value = value.clone();
        let key = rows[index].spec.key.clone();
        self.status = format!("Saving {key}…");
        let _ = self.backend.commands.send(Command::Set { key, value });
    }

    fn reset(&mut self, page: usize, index: usize) {
        let rows = self.rows_for_mut(page);
        rows[index].value = rows[index].spec.default.clone();
        let key = rows[index].spec.key.clone();
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
                ResultMessage::Associations { choices, message } => {
                    self.associations = choices;
                    if let Some(message) = message {
                        self.status = message;
                    }
                }
                ResultMessage::Garden { hidden, message } => {
                    self.hidden_places = hidden;
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
            ASSOCIATIONS_PAGE => Command::Associations(AssociationsCommand::Refresh),
            BACKGROUND_PAGE => Command::Background(BackgroundCommand::Refresh),
            HISTORY_PAGE => Command::History(HistoryCommand::Refresh),
            GARDEN_PAGE => Command::Garden(GardenCommand::Refresh),
            _ => return,
        };
        let _ = self.backend.commands.send(command);
    }

    fn draw_home(&mut self, ui: &mut Ui<'_>, area: Rect) {
        self.draw_key_form(ui, area, HOME_PAGE, HOME_PREFIX);
    }

    /// The Appearance page: light or dark, and the accent. Same form as the Home Field page,
    /// because they are the same kind of thing — a typed key with a control and a Reset.
    fn draw_appearance(&mut self, ui: &mut Ui<'_>, area: Rect) {
        self.draw_key_form(ui, area, APPEARANCE_PAGE, APPEARANCE_PREFIX);
    }

    /// One page of typed settings: a name, the sentence under it, its control and a Reset.
    ///
    /// `page` picks both the scroll state and which list of rows is being edited, so a change on
    /// the Appearance page can never write an index into the Home Field's list.
    fn draw_key_form(&mut self, ui: &mut Ui<'_>, area: Rect, page: usize, prefix: &str) {
        let rows = self.rows_for(page).to_vec();
        let heights = vec![ui.px(88); rows.len()];
        let mut wanted: Option<(usize, Option<SettingValue>)> = None;
        let mut scroll = self.scroll[page];
        ui.scroll_list(
            &mut scroll,
            area,
            &heights,
            |ui, index, rect| match key_form_row(ui, &rows[index], rect, prefix) {
                Some(KeyEdit::Set(value)) => wanted = Some((index, Some(value))),
                Some(KeyEdit::Reset) => wanted = Some((index, None)),
                None => {}
            },
        );
        self.scroll[page] = scroll;
        match wanted {
            Some((index, Some(value))) => self.change(page, index, value),
            Some((index, None)) => self.reset(page, index),
            None => {}
        }
    }

    /// The Homegarden page: the switch behind Home's grown places, the directories activity in
    /// which never grows one, and the places a person has hidden.
    ///
    /// One scrolling list rather than three stacked panes: the ignore list and the hidden places
    /// are both as long as a person has made them, and a page with three independently-scrolling
    /// regions is a page where the thing you are looking for is off the bottom of one of them.
    #[allow(clippy::too_many_lines)]
    fn draw_garden(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let rows = self.garden.clone();
        let ignore_index = rows
            .iter()
            .position(|row| row.spec.key == GARDEN_IGNORE_KEY);
        let ignored: Vec<String> = ignore_index
            .and_then(|index| match &rows[index].value {
                SettingValue::StrList(list) => Some(list.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let hidden = self.hidden_places.clone();

        // The page's rows, in order, with the heights the list scrolls over.
        let mut plan: Vec<GardenRow> = (0..rows.len())
            .filter(|index| Some(*index) != ignore_index)
            .map(GardenRow::Key)
            .collect();
        if ignore_index.is_some() {
            plan.push(GardenRow::Heading("Never grow a place here"));
            plan.extend((0..ignored.len()).map(GardenRow::Ignored));
            plan.push(GardenRow::AddIgnored);
        }
        plan.push(GardenRow::Heading("Hidden places"));
        if hidden.is_empty() {
            plan.push(GardenRow::NothingHidden);
        } else {
            plan.extend(hidden.iter().cloned().map(GardenRow::Hidden));
        }
        let heights: Vec<i32> = plan
            .iter()
            .map(|row| match row {
                GardenRow::Key(_) => ui.px(88),
                GardenRow::NothingHidden => ui.px(32),
                GardenRow::Heading(_)
                | GardenRow::Ignored(_)
                | GardenRow::AddIgnored
                | GardenRow::Hidden(_) => ui.px(44),
            })
            .collect();

        let mut wanted: Option<GardenAction> = None;
        let mut scroll = self.scroll[GARDEN_PAGE];
        let draft = &mut self.ignore_draft;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            match &plan[index] {
                GardenRow::Key(key) => {
                    wanted = match key_form_row(ui, &rows[*key], rect, GARDEN_PREFIX) {
                        Some(KeyEdit::Set(value)) => Some(GardenAction::Set(*key, value)),
                        Some(KeyEdit::Reset) => Some(GardenAction::Reset(*key)),
                        None => wanted.take(),
                    };
                }
                GardenRow::Heading(title) => {
                    ui.label(
                        Rect::new(rect.x, rect.y + ui.px(14), rect.w, ui.px(24)),
                        title,
                        ui.theme.text,
                        Align::Left,
                    );
                }
                GardenRow::NothingHidden => {
                    ui.label(
                        Rect::new(rect.x, rect.y, rect.w, ui.px(24)),
                        "Nothing is hidden. Hiding a place from its tile puts it here.",
                        ui.theme.text_dim,
                        Align::Left,
                    );
                }
                GardenRow::Ignored(entry) => {
                    let row = list_row(ui, rect, &ignored[*entry]);
                    if ui.button(row, "Remove") {
                        wanted = Some(GardenAction::RemoveIgnored(*entry));
                    }
                }
                GardenRow::AddIgnored => {
                    if let Some(entry) =
                        add_row(ui, rect, draft, "a directory name, or an absolute path")
                    {
                        wanted = Some(GardenAction::AddIgnored(entry));
                    }
                }
                GardenRow::Hidden(path) => {
                    let row = list_row(ui, rect, path);
                    if ui.button(row, "Unhide") {
                        wanted = Some(GardenAction::Unhide(path.clone()));
                    }
                }
            }
        });
        self.scroll[GARDEN_PAGE] = scroll;

        let Some(action) = wanted else { return };
        match action {
            GardenAction::Set(index, value) => self.change(GARDEN_PAGE, index, value),
            GardenAction::Reset(index) => self.reset(GARDEN_PAGE, index),
            GardenAction::AddIgnored(entry) => {
                let Some(index) = ignore_index else { return };
                let mut list = ignored;
                if !list.contains(&entry) {
                    list.push(entry);
                    list.sort();
                }
                self.ignore_draft.clear();
                self.change(GARDEN_PAGE, index, SettingValue::StrList(list));
            }
            GardenAction::RemoveIgnored(entry) => {
                let Some(index) = ignore_index else { return };
                let mut list = ignored;
                list.remove(entry);
                self.change(GARDEN_PAGE, index, SettingValue::StrList(list));
            }
            GardenAction::Unhide(path) => {
                self.status = format!("Unhiding {path}…");
                let _ = self
                    .backend
                    .commands
                    .send(Command::Garden(GardenCommand::Unhide(path)));
            }
        }
    }

    /// The Rails page: how the rails are revealed, which side is which, the gate's size, and the
    /// applications under which the gate stays disarmed.
    ///
    /// The exceptions are a list section after the typed keys, for the reason the Homegarden
    /// ignore list is: a list of ids does not fit the key form's control rectangle.
    fn draw_rails(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let rows = self.rails.clone();
        let list_index = rows
            .iter()
            .position(|row| row.spec.key == RAILS_EXCEPTIONS_KEY);
        let exceptions: Vec<String> = list_index
            .and_then(|index| match &rows[index].value {
                SettingValue::StrList(list) => Some(list.clone()),
                _ => None,
            })
            .unwrap_or_default();

        let mut plan: Vec<RailsRow> = (0..rows.len())
            .filter(|index| Some(*index) != list_index)
            .map(RailsRow::Key)
            .collect();
        if list_index.is_some() {
            plan.push(RailsRow::Heading);
            plan.extend((0..exceptions.len()).map(RailsRow::Exception));
            plan.push(RailsRow::AddException);
        }
        let heights: Vec<i32> = plan
            .iter()
            .map(|row| match row {
                RailsRow::Key(_) => ui.px(88),
                RailsRow::Heading | RailsRow::Exception(_) | RailsRow::AddException => ui.px(44),
            })
            .collect();

        let mut wanted: Option<RailsAction> = None;
        let mut scroll = self.scroll[RAILS_PAGE];
        let draft = &mut self.exception_draft;
        let names = &self.names;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            match &plan[index] {
                RailsRow::Key(key) => match key_form_row(ui, &rows[*key], rect, RAILS_PREFIX) {
                    Some(KeyEdit::Set(value)) => wanted = Some(RailsAction::Set(*key, value)),
                    Some(KeyEdit::Reset) => wanted = Some(RailsAction::Reset(*key)),
                    None => {}
                },
                RailsRow::Heading => {
                    ui.label(
                        Rect::new(rect.x, rect.y + ui.px(14), rect.w, ui.px(24)),
                        "Never reveal the rails over",
                        ui.theme.text,
                        Align::Left,
                    );
                }
                RailsRow::Exception(entry) => {
                    // The id is what the compositor matches; the name is what a person knows.
                    let row = list_row(ui, rect, &names.qualified(&exceptions[*entry]));
                    if ui.button(row, "Remove") {
                        wanted = Some(RailsAction::Remove(*entry));
                    }
                }
                RailsRow::AddException => {
                    if let Some(entry) = add_row(
                        ui,
                        rect,
                        draft,
                        "an application id, e.g. org.templaros.Files",
                    ) {
                        wanted = Some(RailsAction::Add(entry));
                    }
                }
            }
        });
        self.scroll[RAILS_PAGE] = scroll;

        let Some(action) = wanted else { return };
        match action {
            RailsAction::Set(index, value) => self.change(RAILS_PAGE, index, value),
            RailsAction::Reset(index) => self.reset(RAILS_PAGE, index),
            RailsAction::Add(entry) => {
                let Some(index) = list_index else { return };
                let mut list = exceptions;
                if !list.contains(&entry) {
                    list.push(entry);
                    list.sort();
                }
                self.exception_draft.clear();
                self.change(RAILS_PAGE, index, SettingValue::StrList(list));
            }
            RailsAction::Remove(entry) => {
                let Some(index) = list_index else { return };
                let mut list = exceptions;
                list.remove(entry);
                self.change(RAILS_PAGE, index, SettingValue::StrList(list));
            }
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

        // Choices rather than a track: seven named delays a person picks from, because "how long
        // before the screen turns off" is a decision with a handful of sensible answers and not a
        // continuum. The row is the widest on the page, so it gets its own controls rect.
        let screen_off_line = crate::system::screen_off_line(&self.system);
        let selected = crate::system::screen_off_seconds(&self.system);
        let row = Rect::new(
            area.x,
            area.y + ui.px(88) * 3,
            area.w,
            ui.px(78).min(area.h),
        );
        ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
        ui.painter
            .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);
        ui.label(
            Rect::new(
                row.x + ui.px(12),
                row.y + ui.px(7),
                row.w - ui.px(24),
                ui.px(24),
            ),
            "Screen turns off after",
            ui.theme.text,
            Align::Left,
        );
        ui.label(
            Rect::new(
                row.x + ui.px(12),
                row.y + ui.px(33),
                row.w - ui.px(24),
                ui.px(20),
            ),
            &screen_off_line,
            ui.theme.text_dim,
            Align::Left,
        );
        if let Some(seconds) = screen_off_choice(ui, row, selected) {
            self.system(SystemCommand::SetScreenOff(seconds));
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
        let dropdown = &mut self.chooser;
        let names = &self.names;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let choice = &choices[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(74));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            // Wide enough for a real application name and its priority. At 300 the File Manager
            // row read "PCManFM-Qt File Manag…" and the priority the sentence beside it explains
            // was off the end of the field.
            let field_w = ui.px(380);
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

            let labels = crate::providers::ChoiceOption::labels(&choice.options, names);
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
            self.chooser_row = self.chooser.is_open().then_some(index);
        }
    }

    /// The open chooser's popup, drawn after every page so nothing covers it, and the pick it
    /// reports applied to the row that opened it.
    fn draw_chooser_popup(&mut self, ui: &mut Ui<'_>, bounds: Rect) {
        let mut dropdown = std::mem::take(&mut self.chooser);
        let picked = dropdown.draw(ui, bounds);
        self.chooser = dropdown;
        let (Some(picked), Some(row)) = (picked, self.chooser_row) else {
            return;
        };
        self.chooser_row = None;
        let command = match self.selected_page {
            PROVIDERS_PAGE => self.provider_pick(row, picked).map(Command::Providers),
            ASSOCIATIONS_PAGE => self
                .association_pick(row, picked)
                .map(Command::Associations),
            _ => None,
        };
        if let Some(command) = command {
            "Saving…".clone_into(&mut self.status);
            let _ = self.backend.commands.send(command);
        }
    }

    fn provider_pick(&self, row: usize, picked: usize) -> Option<ProvidersCommand> {
        let choice = self.providers.get(row)?;
        let role = choice.role.to_string();
        Some(match choice.options.get(picked)? {
            ChoiceOption::Automatic => ProvidersCommand::Deselect { role },
            ChoiceOption::App { app_id, .. } => ProvidersCommand::Select {
                role,
                app_id: app_id.clone(),
            },
        })
    }

    fn association_pick(&self, row: usize, picked: usize) -> Option<AssociationsCommand> {
        let choice = self.associations.get(row)?;
        let mime = choice.mime.clone();
        let option = choice.options.get(picked)?;
        Some(if option == crate::associations::ASK_EACH_TIME {
            AssociationsCommand::ForgetMime { mime }
        } else {
            AssociationsCommand::SetMimeDefault {
                mime,
                app_id: option.clone(),
            }
        })
    }

    /// The Associations page: which application opens each type that has a default set, and a
    /// chooser holding exactly the applications the engine will accept for it.
    ///
    /// Only types that already have a default. The shared MIME database runs to thousands of
    /// types, and this page is where a person revisits a choice they made — the first choice for
    /// a type is made where the file is.
    fn draw_associations(&mut self, ui: &mut Ui<'_>, area: Rect) {
        if self.associations.is_empty() {
            ui.label(
                Rect::new(area.x, area.y, area.w, ui.px(24)),
                "No file type has a default application set yet.",
                ui.theme.text_dim,
                Align::Left,
            );
            return;
        }
        let choices = self.associations.clone();
        let heights = vec![ui.px(56); choices.len()];
        let mut opened: Option<usize> = None;
        let mut scroll = self.scroll[ASSOCIATIONS_PAGE];
        let dropdown = &mut self.chooser;
        let names = &self.names;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let choice = &choices[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(48));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            let field_w = ui.px(300);
            let text_w = (row.w - field_w - ui.px(36)).max(ui.px(120));
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(13), text_w, ui.px(22)),
                &choice.mime,
                ui.theme.text,
                Align::Left,
            );
            let labels: Vec<String> = choice
                .options
                .iter()
                .map(|option| {
                    if option == crate::associations::ASK_EACH_TIME {
                        option.clone()
                    } else {
                        names.qualified(option)
                    }
                })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let field = Rect::new(
                row.right() - field_w - ui.px(12),
                row.y + ui.px(7),
                field_w,
                ui.px(34),
            );
            if ui.dropdown(dropdown, field, &refs, choice.selected) {
                opened = Some(index);
            }
        });
        self.scroll[ASSOCIATIONS_PAGE] = scroll;
        if let Some(index) = opened {
            self.chooser_row = self.chooser.is_open().then_some(index);
        }
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
        let names = &self.names;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let entry = &entries[index];
            let row = Rect::new(rect.x, rect.y, rect.w, ui.px(86));
            ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
            ui.painter
                .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);

            let buttons = ui.px(168);
            let text_w = (row.w - buttons - ui.px(24)).max(ui.px(120));
            // Qualified, not just the name: granting background activity grants it to one
            // installed copy, and two copies of Okular that both read "Okular" would make the
            // row a coin toss.
            ui.label(
                Rect::new(row.x + ui.px(12), row.y + ui.px(6), text_w, ui.px(24)),
                &names.qualified(&entry.app_id),
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
        let names = self.names.clone();
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
                &event.detail(now, &names),
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
/// One row of a typed-key form: the name, the sentence under it, its control and a Reset.
///
/// `None` is a frame in which nothing was clicked. Shared by every page of typed keys so they all
/// read the same, and so another page costs a call rather than a copy.
fn key_form_row(ui: &mut Ui<'_>, row: &SettingRow, rect: Rect, prefix: &str) -> Option<KeyEdit> {
    let row_area = Rect::new(rect.x, rect.y, rect.w, ui.px(78));
    ui.painter
        .fill_rounded_rect(row_area, ui.px(6), ui.theme.panel);
    ui.painter
        .stroke_rect(row_area, ui.scale.hairline(), ui.theme.panel_edge);
    let name = row
        .spec
        .key
        .strip_prefix(prefix)
        .unwrap_or(&row.spec.key)
        .replace(['-', '.'], " ");
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
        &row.spec.description,
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
        return Some(KeyEdit::Reset);
    }
    home_control(ui, row, controls).map(KeyEdit::Set)
}

/// What a click on one typed-key row asked for.
#[derive(Clone, Debug)]
enum KeyEdit {
    Set(SettingValue),
    Reset,
}

/// One row of a plain list of paths: the path, and the rectangle its single button gets.
fn list_row(ui: &mut Ui<'_>, rect: Rect, path: &str) -> Rect {
    let row = Rect::new(rect.x, rect.y, rect.w, ui.px(36));
    ui.painter.fill_rounded_rect(row, ui.px(6), ui.theme.panel);
    ui.painter
        .stroke_rect(row, ui.scale.hairline(), ui.theme.panel_edge);
    let button = Rect::new(
        row.right() - ui.px(88),
        row.y + ui.px(3),
        ui.px(80),
        ui.px(30),
    );
    ui.label(
        Rect::new(
            row.x + ui.px(12),
            row.y,
            (button.x - row.x - ui.px(20)).max(0),
            row.h,
        ),
        path,
        ui.theme.text,
        Align::Left,
    );
    button
}

/// One row of the Homegarden page. The page is three sections in one scrolling list, so a row is
/// a typed key, a heading, or an entry in one of the two lists.
#[derive(Clone, Debug)]
enum GardenRow {
    Key(usize),
    Heading(&'static str),
    Ignored(usize),
    AddIgnored,
    Hidden(String),
    NothingHidden,
}

/// What a frame's input asked of the Homegarden page.
#[derive(Clone, Debug)]
enum GardenAction {
    Set(usize, SettingValue),
    Reset(usize),
    AddIgnored(String),
    RemoveIgnored(usize),
    Unhide(String),
}

/// One row of the Rails page: a typed key, the exceptions heading, one exception, or the field
/// that adds one.
#[derive(Clone, Debug)]
enum RailsRow {
    Key(usize),
    Heading,
    Exception(usize),
    AddException,
}

/// What a frame's input asked of the Rails page.
#[derive(Clone, Debug)]
enum RailsAction {
    Set(usize, SettingValue),
    Reset(usize),
    Add(String),
    Remove(usize),
}

/// The last row of an editable list: a text field and an Add button. Returns the trimmed entry
/// when Add is pressed with something typed, so an empty click adds nothing.
fn add_row(
    ui: &mut Ui<'_>,
    rect: Rect,
    draft: &mut pixelkit_ui::TextFieldState,
    placeholder: &str,
) -> Option<String> {
    let button = Rect::new(
        rect.right() - ui.px(84),
        rect.y + ui.px(2),
        ui.px(84),
        ui.px(34),
    );
    let field = Rect::new(
        rect.x,
        button.y,
        (button.x - rect.x - ui.px(8)).max(ui.px(80)),
        button.h,
    );
    ui.text_field(draft, field, placeholder, true, false);
    let entry = draft.text().trim();
    (ui.button(button, "Add") && !entry.is_empty()).then(|| entry.to_owned())
}

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

/// The seven named screen-off delays as one strip of buttons, right-aligned in `row`. Returns the
/// one that was pressed.
///
/// `selected` is `None` when `Power1` did not answer: nothing is drawn as chosen, because with no
/// stored value every choice on the strip would be a guess, and a settings page that guesses is
/// worse than one that says it does not know. The row's own sentence says which it is.
fn screen_off_choice(ui: &mut Ui<'_>, row: Rect, selected: Option<u32>) -> Option<u32> {
    let gap = ui.px(6);
    let width = ui.px(62);
    let strip_width = width * crate::system::SCREEN_OFF_CHOICES.len() as i32
        + gap * (crate::system::SCREEN_OFF_CHOICES.len() as i32 - 1);
    let mut x = row.right() - ui.px(12) - strip_width;
    let y = row.bottom() - ui.px(12) - ui.px(30);
    let mut pressed = None;
    for (label, seconds) in crate::system::SCREEN_OFF_CHOICES {
        let button = Rect::new(x, y, width, ui.px(30));
        x += width + gap;
        if selected == Some(seconds) {
            ui.painter
                .fill_rounded_rect(button, ui.px(6), ui.theme.accent);
            ui.label(button, label, ui.theme.panel, Align::Centre);
        } else if selected.is_none() {
            // Inert and dimmed rather than absent: a control that vanishes leaves a person
            // hunting for it (`level_controls`' own reasoning).
            ui.label(button, label, ui.theme.text_dim, Align::Centre);
        } else if ui.button(button, label) {
            pressed = Some(seconds);
        }
    }
    pressed
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
                // A popup belonging to the page just left would otherwise hang over the new one
                // and apply its pick to a row that is no longer there.
                self.chooser.close();
                self.chooser_row = None;
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
                ASSOCIATIONS_PAGE => self.draw_associations(&mut ui, content),
                BACKGROUND_PAGE => self.draw_background(&mut ui, content),
                HISTORY_PAGE => self.draw_history(&mut ui, content),
                APPEARANCE_PAGE => self.draw_appearance(&mut ui, content),
                GARDEN_PAGE => self.draw_garden(&mut ui, content),
                RAILS_PAGE => self.draw_rails(&mut ui, content),
                _ => self.draw_text_page(&mut ui, content),
            }
            // Last, so nothing the page drew afterwards covers the open list.
            self.draw_chooser_popup(&mut ui, content);
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
                "../../../shell/templar-shell/assets/fonts/DejaVuSans.ttf"
            ))
            .expect("embedded DejaVu Sans"),
        ));
        let mut fonts = FontSet::new();
        fonts.add(face);
        fonts
    })
}

/// Which page of typed controls a schema key belongs on, or `None` for a key no page shows.
///
/// The prefixes are disjoint by construction, and this is where that is stated once: a key
/// appearing on two pages, or on none, is a control a person either sees twice or cannot find at
/// all. `shell.home-garden.*` used to fall through to `None`, which is why Home's default-on
/// automation had no graphical settings at all.
fn page_for(key: &str) -> Option<usize> {
    if key.starts_with(HOME_PREFIX) {
        Some(HOME_PAGE)
    } else if key.starts_with(APPEARANCE_PREFIX) {
        Some(APPEARANCE_PAGE)
    } else if key.starts_with(GARDEN_PREFIX) {
        Some(GARDEN_PAGE)
    } else if key.starts_with(RAILS_PREFIX) {
        Some(RAILS_PAGE)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)]
async fn load() -> Result<SettingsApp> {
    let connection = zbus::Connection::session().await.context("session bus")?;
    let proxy = SettingsProxy::new(&connection).await?;
    let values: std::collections::HashMap<_, _> = proxy.list().await?.into_iter().collect();
    let (mut home, mut appearance, mut garden, mut rails) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for json in proxy.schema_details().await? {
        let spec: SettingSpec = serde_json::from_str(&json)?;
        let list = match page_for(&spec.key) {
            Some(APPEARANCE_PAGE) => &mut appearance,
            Some(GARDEN_PAGE) => &mut garden,
            Some(RAILS_PAGE) => &mut rails,
            Some(_) => &mut home,
            None => continue,
        };
        let value = values
            .get(&spec.key)
            .map(|json| serde_json::from_str(json))
            .transpose()?
            .unwrap_or_else(|| spec.default.clone());
        list.push(SettingRow { spec, value });
    }
    for list in [&mut home, &mut appearance, &mut garden, &mut rails] {
        list.sort_by(|left, right| left.spec.key.cmp(&right.spec.key));
    }

    // One entry per tab, so `selected_page` indexes both. The four interactive pages draw their
    // own controls and keep an empty entry here rather than a rendered copy of themselves.
    let pages = vec![
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
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
        Vec::new(),
        Vec::new(),
        Vec::new(),
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
    let providers = match templar_client::proxies::ProvidersProxy::new(&connection).await {
        Ok(proxy) => crate::providers::role_choices_with(&proxy)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let names = crate::names::load(&connection).await;
    let associations = match templar_client::proxies::AssociationsProxy::new(&connection).await {
        // Without the per-type handler fetch: the page is opened by a tab click, and that click
        // already refreshes it in full on the worker thread.
        Ok(proxy) => crate::associations::mime_choices_with(&proxy, false)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let history = match templar_client::proxies::HistoryProxy::new(&connection).await {
        Ok(proxy) => proxy
            .recent(HISTORY_LIMIT)
            .await
            .map(|raw| crate::history::parse_events(&raw))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    // The garden may not be running (`shell.home-garden.enabled` off, or an older sessiond);
    // an empty list of hidden places is then the truth, not a failure.
    let hidden_places = crate::garden::fetch_hidden(&connection)
        .await
        .unwrap_or_default();

    Ok(SettingsApp {
        home,
        appearance,
        garden,
        rails,
        hidden_places,
        ignore_draft: pixelkit_ui::TextFieldState::new(),
        exception_draft: pixelkit_ui::TextFieldState::new(),
        system,
        background,
        history,
        providers,
        associations,
        names,
        chooser: Dropdown::new(),
        chooser_row: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each page of typed controls lists its own keys and nobody else's, checked against the
    /// real schema rather than a list written out here — a key added under the wrong prefix is
    /// a control that appears on the wrong page, or on none.
    #[test]
    fn appearance_page_lists_only_appearance_keys() {
        let schema = templar_session_core::settings::baseline_schema();
        let mut appearance = Vec::new();
        let mut home = Vec::new();
        let mut garden = Vec::new();
        let mut rails = Vec::new();
        for key in schema.specs.keys() {
            match page_for(key) {
                Some(APPEARANCE_PAGE) => appearance.push(key.as_str()),
                Some(HOME_PAGE) => home.push(key.as_str()),
                Some(GARDEN_PAGE) => garden.push(key.as_str()),
                Some(RAILS_PAGE) => rails.push(key.as_str()),
                Some(other) => panic!("{key} claims page {other}, which draws no key form"),
                None => {}
            }
        }
        assert_eq!(
            appearance,
            vec!["shell.appearance.accent", "shell.appearance.theme"],
            "the Appearance page shows the wrong set of keys"
        );
        assert!(!home.is_empty(), "the Home Field page lost its keys");
        for key in &home {
            assert!(!appearance.contains(key), "{key} is on both pages");
        }
        // Every key either page shows has to be a real schema key with a default, or its Reset
        // has nothing to reset to.
        for key in appearance.iter().chain(&home).chain(&garden).chain(&rails) {
            assert!(schema.specs.contains_key(*key));
        }
    }

    /// Finding 6 of the Homegarden usability review: "Settings excludes every `shell.home-garden.*`
    /// key". Every one of them now reaches the Homegarden page, and the one the key form cannot
    /// draw is the one that page gives a section of its own.
    #[test]
    fn every_home_garden_key_reaches_the_homegarden_page() {
        let schema = templar_session_core::settings::baseline_schema();
        let garden: Vec<&str> = schema
            .specs
            .keys()
            .filter(|key| key.starts_with(GARDEN_PREFIX))
            .map(String::as_str)
            .collect();
        assert!(!garden.is_empty(), "the schema has no garden keys");
        for key in &garden {
            assert_eq!(page_for(key), Some(GARDEN_PAGE), "{key} has no page");
        }
        assert!(
            garden.contains(&GARDEN_IGNORE_KEY),
            "the ignore list the page draws by hand is not a schema key"
        );
        assert!(
            matches!(
                schema.specs[GARDEN_IGNORE_KEY].default,
                SettingValue::StrList(_)
            ),
            "the ignore section assumes a string list"
        );
    }

    /// Every `shell.rails.*` key reaches the Rails page, and the exceptions list the page draws
    /// by hand is a real key holding the type that section edits.
    #[test]
    fn every_rails_key_reaches_the_rails_page() {
        let schema = templar_session_core::settings::baseline_schema();
        let rails: Vec<&str> = schema
            .specs
            .keys()
            .filter(|key| key.starts_with(RAILS_PREFIX))
            .map(String::as_str)
            .collect();
        assert_eq!(
            rails,
            vec![
                "shell.rails.exceptions",
                "shell.rails.gate-height",
                "shell.rails.gate-width",
                "shell.rails.learning",
                "shell.rails.mode",
                "shell.rails.swap",
            ],
            "the Rails page shows the wrong set of keys"
        );
        for key in &rails {
            assert_eq!(page_for(key), Some(RAILS_PAGE), "{key} has no page");
        }
        assert!(
            matches!(
                schema.specs[RAILS_EXCEPTIONS_KEY].default,
                SettingValue::StrList(_)
            ),
            "the exceptions section assumes a string list"
        );
    }

    /// The tab list and the page indices are two halves of one thing; a page index past the end
    /// of the names is a tab a person can never reach.
    #[test]
    fn every_named_page_index_is_within_the_tab_list() {
        for page in [
            HOME_PAGE,
            SYSTEM_PAGE,
            PROVIDERS_PAGE,
            ASSOCIATIONS_PAGE,
            BACKGROUND_PAGE,
            HISTORY_PAGE,
            APPEARANCE_PAGE,
            GARDEN_PAGE,
            RAILS_PAGE,
        ] {
            assert!(page < PAGE_NAMES.len(), "page {page} has no tab");
        }
        assert_eq!(PAGE_NAMES[APPEARANCE_PAGE], "Appearance");
        assert_eq!(PAGE_NAMES[GARDEN_PAGE], "Homegarden");
        assert_eq!(PAGE_NAMES[RAILS_PAGE], "Rails");
    }
}
