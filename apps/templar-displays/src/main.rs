//! Standalone Lubuntu/X11 page. No Alpha/sessiond dependency and no GUI-owned rollback timer.
mod controller;
mod preview;
mod randr;
use anyhow::{Context, Result};
use clap::Parser;
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, Modifiers, MouseButton, Scale};
use pixelkit_text::{Face, FontSet, TextCache};
use pixelkit_ui::{Focus, Ui};
use pixelkit_windowing::{PixelApp, Waker, WindowConfig, run_app};
use std::{
    sync::{Arc, LazyLock, Mutex, mpsc},
    time::Duration,
};
use templar_display_ui::{DisplaysCommand, DisplaysState, DisplaysView};
#[derive(Parser)]
#[command(
    name = "templar-displays",
    version,
    about = "Standalone actual X11 RandR display editor with independent preview rollback"
)]
struct Args {
    #[arg(long)]
    print: bool,
    #[arg(long, hide = true)]
    rollback_helper: bool,
}
static FACE: LazyLock<Face> = LazyLock::new(|| {
    Face::from_bytes(include_bytes!(
        "../../../shell/templar-shell/assets/fonts/DejaVuSans.ttf"
    ))
    .expect("embedded DejaVu Sans")
});
static FONTS: LazyLock<FontSet> = LazyLock::new(|| {
    let mut fonts = FontSet::new();
    fonts.add(&FACE);
    fonts
});
fn fonts() -> &'static FontSet {
    &FONTS
}
struct App {
    view: DisplaysView,
    text: TextCache,
    input: Input,
    kernel: RasterKernel,
    focus: Focus,
    appearance: templar_ui::AppearanceState,
    appearance_observer: templar_client::appearance::AppearanceObserver,
    appearance_service_seen: bool,
    semantics: pixelkit_ui::semantics::Semantics,
    semantics_active: bool,
    commands: mpsc::SyncSender<DisplaysCommand>,
    results: mpsc::Receiver<DisplaysState>,
    waker: Arc<Mutex<Waker>>,
    animating: bool,
}
impl App {
    fn new() -> Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(4);
        let (sender, results) = mpsc::channel();
        let waker = Arc::new(Mutex::new(Waker::detached()));
        let worker_waker = Arc::clone(&waker);
        let appearance_waker = Arc::clone(&waker);
        let appearance_observer = templar_client::appearance::AppearanceObserver::new(move || {
            appearance_waker.lock().expect("waker mutex").wake()
        });
        std::thread::Builder::new()
            .name("standalone-randr-controller".into())
            .spawn(move || {
                let mut controller = match controller::Controller::new() {
                    Ok(controller) => controller,
                    Err(error) => {
                        let _ = sender.send(DisplaysState {
                            observation: None,
                            message: Some(format!(
                                "Actual X11 display source unavailable: {error:#}"
                            )),
                        });
                        worker_waker.lock().expect("waker mutex").wake();
                        return;
                    }
                };
                let initial = controller.run(DisplaysCommand::Refresh);
                if sender.send(initial).is_err() {
                    return;
                }
                worker_waker.lock().expect("waker mutex").wake();
                loop {
                    let command = match receiver.recv_timeout(Duration::from_millis(200)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => match controller.changed() {
                            Ok(false) => continue,
                            Ok(true) => DisplaysCommand::Refresh,
                            Err(error) => {
                                if sender
                                    .send(DisplaysState {
                                        observation: None,
                                        message: Some(format!(
                                            "Actual RandR event source failed: {error:#}"
                                        )),
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                                worker_waker.lock().expect("waker mutex").wake();
                                continue;
                            }
                        },
                    };
                    let state = controller.run(command);
                    if sender.send(state).is_err() {
                        break;
                    }
                    worker_waker.lock().expect("waker mutex").wake();
                }
                // Dropping only our pipe tells the independent owner to restore; never kill it here.
            })
            .context("Start the actual display controller")?;
        Ok(Self {
            view: DisplaysView::default(),
            text: TextCache::new(fonts()),
            input: Input::new(),
            kernel: RasterKernel::new(),
            focus: Focus::new(),
            appearance: templar_ui::AppearanceState::default(),
            appearance_observer,
            appearance_service_seen: false,
            commands,
            results,
            waker,
            animating: false,
            semantics: Default::default(),
            semantics_active: false,
        })
    }
    fn drain(&mut self) {
        while let Ok(state) = self.results.try_recv() {
            self.view.replace(state);
            self.input.clear_accessible_text_edit();
            self.semantics.invalidate();
        }
        if let Some(Ok(appearance)) = self.appearance_observer.take_update() {
            self.appearance_service_seen = true;
            if self.appearance.update(appearance) {
                self.input.clear_accessible_text_edit();
                self.semantics.invalidate();
            }
        }
    }
}
impl PixelApp for App {
    fn render(&mut self, buffer: &mut WindowBuffer, scale: Scale) {
        self.drain();
        let whole = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
        if self.semantics_active {
            self.semantics.begin("Displays");
        }
        let command;
        {
            let ui = Ui::new(
                Painter::new(buffer),
                &mut self.text,
                &mut self.input,
                self.appearance.theme(),
                scale,
                &mut self.kernel,
            )
            .with_focus(&mut self.focus);
            let mut ui = if self.semantics_active {
                ui.with_semantics(&mut self.semantics)
            } else {
                ui
            };
            ui.painter.fill_rect(whole, ui.theme.background);
            let inset = whole.inset(ui.px(18));
            let heading = Rect::new(inset.x, inset.y, inset.w, ui.px(40));
            ui.heading(heading, "Displays");
            let content = Rect::new(
                inset.x,
                heading.bottom() + ui.px(8),
                inset.w,
                inset.bottom() - heading.bottom() - ui.px(8),
            );
            let result = self.view.draw(&mut ui, content);
            command = result.0;
            self.animating = result.1;
        }
        self.input.end_frame();
        if let Some(command) = command {
            match self.commands.try_send(command) {Ok(())=>{},Err(mpsc::TrySendError::Full(_))=>self.view.state.message=Some("The controller is busy; this request was not submitted".into()),Err(mpsc::TrySendError::Disconnected(_))=>self.view.state.message=Some("Actual display controller ended; any pending preview is still owned by its independent rollback process".into())}
        }
    }
    fn attach(&mut self, waker: Waker) {
        let clipboard_waker = waker.clone();
        self.input
            .attach_clipboard(pixelkit_shell::clipboard::ClipboardWorker::new(move || {
                clipboard_waker.wake()
            }));
        *self.waker.lock().expect("waker mutex") = waker;
    }
    fn animation_interval(&self) -> Option<Duration> {
        self.animating.then_some(Duration::from_millis(100))
    }
    fn tick(&mut self) {
        self.drain();
    }
    fn on_key(&mut self, key: &KeyInput) {
        if !matches!(key, KeyInput::Character(_)) {
            self.input.key(key.clone());
        }
    }
    fn on_key_event(&mut self, event: &pixelkit_windowing::KeyEvent) {
        if event.pressed && matches!(event.key, KeyInput::Character(_)) {
            if event.modifiers.control || event.modifiers.logo {
                self.input.key(event.key.clone());
            } else {
                for character in event.text.chars() {
                    self.input.key(KeyInput::Character(character));
                }
            }
        }
    }
    fn on_ime(&mut self, event: &pixelkit_windowing::ImeEvent) {
        match event {
            pixelkit_windowing::ImeEvent::Preedit(text, _) => self.input.set_composition(text),
            pixelkit_windowing::ImeEvent::Commit(text) => {
                self.input.set_composition("");
                for character in text.chars() {
                    self.input.key(KeyInput::Character(character));
                }
            }
            pixelkit_windowing::ImeEvent::Disabled => self.input.set_composition(""),
            pixelkit_windowing::ImeEvent::Enabled => {}
        }
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
    fn on_focus_changed(&mut self, focused: bool) {
        if !focused {
            self.input.blur();
            self.semantics.invalidate();
        }
    }
    fn on_layout_changed(&mut self) {
        self.input.clear_accessible_text_edit();
        self.semantics.invalidate();
    }
    fn ime_allowed(&self) -> bool {
        self.view.caret_area().is_some()
    }
    fn ime_cursor_area(&self) -> Option<pixelkit_windowing::ImeCursorArea> {
        let area = self.view.caret_area()?;
        Some(pixelkit_windowing::ImeCursorArea {
            x: f64::from(area.x),
            y: f64::from(area.y),
            width: f64::from(area.w),
            height: f64::from(area.h),
        })
    }
    fn accessibility_tree(&mut self) -> Option<pixelkit_ui::semantics::TreeUpdate> {
        self.semantics_active = true;
        Some(self.semantics.update())
    }
    fn on_accessibility_action(&mut self, request: pixelkit_ui::semantics::ActionRequest) {
        self.semantics.action(&mut self.input, request);
    }
    fn on_accessibility_deactivated(&mut self) {
        self.semantics_active = false;
        self.input.clear_accessible_text_edit();
        self.semantics.invalidate();
    }
    fn on_theme(&mut self, dark: bool) {
        if !self.appearance_service_seen {
            self.appearance
                .update(templar_types::appearance::Appearance::from_settings(
                    if dark { "dark" } else { "light" },
                    "default",
                    self.appearance.appearance().reduced_motion,
                ));
        }
    }
}
fn run() -> Result<()> {
    let args = Args::parse();
    if args.rollback_helper {
        return preview::helper();
    }
    if args.print {
        let observation = randr::Randr::connect(uuid::Uuid::new_v4(), 0)?
            .snapshot()?
            .observation;
        println!("{}", serde_json::to_string_pretty(&observation)?);
        return Ok(());
    }
    run_app(
        App::new()?,
        WindowConfig::new("Displays", 1040.0, 800.0).min_size(720.0, 650.0),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
fn main() {
    if let Err(error) = run() {
        eprintln!("templar-displays: {error:#}");
        std::process::exit(1);
    }
}
