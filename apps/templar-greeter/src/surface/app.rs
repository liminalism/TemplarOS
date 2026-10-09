//! Wiring: one `winit::application::ApplicationHandler` hosting the greeter's one full-screen
//! window, `crate::login::LoginController` driving `crate::client::GreetdClient` for it, and
//! `crate::screen::GreeterScreen` turning key/mouse input into `crate::login`/`crate::sessions`
//! actions. See `surface::window`'s module doc for why this crate drives winit directly rather than
//! `pixelkit_shell::run_app` — the fullscreen requirement `WindowConfig` has no way to express.
//!
//! greetd exchanges run on one worker: PAM can wait for a fingerprint for several seconds, while
//! the event loop must still paint its instructions and accept cancellation. Info/error messages
//! are acknowledged automatically only after their text has been presented.

use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc};

use anyhow::Context;
use pixelkit_raster::{Painter, RasterKernel, Rect};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use crate::client::GreetdClient;
use crate::login::{LoginController, LoginFlow};
use crate::screen::{GreeterAction, GreeterData, GreeterScreen};
use crate::sessions::{SessionEntry, SessionKind};
use crate::surface::fonts;
use crate::surface::window::{
    HostedWindow, key_pressed, modifiers_of, scroll_pixels, translate_button, translate_keys,
};

/// Run the greeter surface: connect to greetd, discover the available sessions, open the
/// full-screen window, and block until either a session was started (exit code `0`) or the window
/// closed / a fatal error occurred (non-zero) — `main.rs` exits the process with whatever this
/// returns.
pub fn run() -> anyhow::Result<i32> {
    let client = GreetdClient::connect_from_env().context(
        "connecting to greetd via GREETD_SOCK — this binary must be run as greetd's \
         default_session.command",
    )?;
    let sessions = crate::sessions::discover(&crate::sessions::default_dirs());
    if sessions.is_empty() {
        tracing::warn!(
            "no session .desktop files found under /usr/share/wayland-sessions or \
             /usr/share/xsessions — the picker will show \"No sessions found\" until at least one \
             exists"
        );
    }
    let selected_session = crate::sessions::default_index(&sessions);

    let event_loop = EventLoop::<AuthUpdate>::with_user_event()
        .build()
        .context("creating the winit event loop")?;
    let mut app = GreeterApp::new(
        client,
        sessions,
        selected_session,
        event_loop.create_proxy(),
    );
    event_loop
        .run_app(&mut app)
        .context("running the greeter's event loop")?;
    Ok(app.exit_code.unwrap_or(1))
}

enum AuthCommand {
    Action(GreeterAction),
    Start { cmd: Vec<String>, env: Vec<String> },
}

enum AuthUpdate {
    Flow { flow: LoginFlow, canceled: bool },
    Started(String),
    Wake,
    Accessibility(accesskit_winit::Event),
}
impl From<accesskit_winit::Event> for AuthUpdate {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Accessibility(event)
    }
}

fn authentication_worker(
    client: GreetdClient<UnixStream>,
    commands: mpsc::Receiver<AuthCommand>,
    proxy: &EventLoopProxy<AuthUpdate>,
) {
    let mut controller = LoginController::new(client);
    for command in commands {
        let canceled = matches!(&command, AuthCommand::Action(GreeterAction::Cancel));
        match command {
            AuthCommand::Action(action) => match action {
                GreeterAction::None => continue,
                GreeterAction::SubmitUsername(username) => controller.submit_username(username),
                GreeterAction::SubmitResponse(text) => controller.submit_response(text),
                GreeterAction::Cancel => controller.cancel(),
            },
            AuthCommand::Start { cmd, env } => {
                let username = controller.flow.username().unwrap_or_default().to_owned();
                if controller.start_session(cmd, env) {
                    let _ = proxy.send_event(AuthUpdate::Started(username));
                    return;
                }
            }
        }
        if proxy
            .send_event(AuthUpdate::Flow {
                flow: controller.flow.clone(),
                canceled,
            })
            .is_err()
        {
            return;
        }
    }
}

struct GreeterApp {
    commands: mpsc::Sender<AuthCommand>,
    flow: LoginFlow,
    pending: bool,
    cancel_pending: bool,
    sessions: Vec<SessionEntry>,
    window: Option<HostedWindow>,
    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    theme: Theme,
    screen: GreeterScreen,
    proxy: EventLoopProxy<AuthUpdate>,
    accessibility: Option<accesskit_winit::Adapter>,
    semantics: pixelkit_ui::semantics::Semantics,
    semantics_active: bool,
    native_focused: bool,
    /// `None` until the window should close: `Some(0)` once a session started, `Some(_ != 0)` on
    /// any failure severe enough to give up (the window itself failing to open; the user closing
    /// it). Never set from an ordinary login failure — those stay on-screen, retryable, per
    /// `crate::login`'s whole reason for existing.
    exit_code: Option<i32>,
}

impl GreeterApp {
    fn new(
        client: GreetdClient<UnixStream>,
        sessions: Vec<SessionEntry>,
        selected_session: usize,
        proxy: EventLoopProxy<AuthUpdate>,
    ) -> GreeterApp {
        let mut screen = GreeterScreen {
            selected_session,
            ..GreeterScreen::default()
        };
        if let Some(username) = crate::last_user::load(std::path::Path::new(crate::last_user::PATH))
        {
            screen.username.set_text(&username);
        }
        let (commands, receiver) = mpsc::channel();
        let worker_proxy = proxy.clone();
        std::thread::spawn(move || authentication_worker(client, receiver, &worker_proxy));
        GreeterApp {
            commands,
            flow: LoginFlow::idle(),
            pending: false,
            cancel_pending: false,
            sessions,
            window: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            theme: templar_ui::theme_for(&templar_types::appearance::LIGHT),
            screen,
            proxy,
            accessibility: None,
            semantics: pixelkit_ui::semantics::Semantics::default(),
            semantics_active: false,
            native_focused: true,
            exit_code: None,
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = &mut self.window else {
            return;
        };
        if !window.prepare() {
            return;
        }
        let scale = window.scale;
        let (width, height) = (window.buffer.width as i32, window.buffer.height as i32);
        // pixelkit's widgets paint first and apply the frame's input after (`Ui::text_field` draws
        // the field, then `consume_keys`), so a frame that took keys shows the state from *before*
        // them. Without one more frame the field lags a key behind until the next event: type
        // "dk" and it shows "d" — on a login screen, indistinguishable from dropped keys.
        let had_keys = !window.input.keys().is_empty();
        let data = GreeterData {
            flow: &self.flow,
            sessions: &self.sessions,
        };
        let action = {
            if self.semantics_active {
                self.semantics.begin_scoped("TemplarOS Login", 0);
            }
            let painter = Painter::new(&mut window.buffer);
            let ui = Ui::new(
                painter,
                &mut self.text,
                &mut window.input,
                self.theme,
                scale,
                &mut self.kernel,
            );
            let mut ui = if self.semantics_active {
                ui.with_semantics(&mut self.semantics)
            } else {
                ui
            };
            let area = Rect::new(0, 0, width, height);
            let action = self.screen.draw(&mut ui, area, &data);
            // The session picker's open popup, if any, draws last — `Dropdown::draw`'s own
            // contract (`pixelkit-ui/src/dropdown.rs`).
            self.screen.draw_session_popup(&mut ui, area);
            action
        };
        window.input.end_frame();
        window.present();
        let caret = if self.pending || !self.native_focused {
            None
        } else {
            match &self.flow {
                LoginFlow::Idle { .. } => self.screen.username.caret_area(),
                LoginFlow::Authenticating { prompt, .. } if prompt.needs_response => {
                    self.screen.response.caret_area()
                }
                _ => None,
            }
        };
        if let Some(area) = caret {
            pixelkit_windowing::host::apply_ime(
                &window.window,
                Some(pixelkit_windowing::ImeCursorArea {
                    x: f64::from(area.x),
                    y: f64::from(area.y),
                    width: f64::from(area.w),
                    height: f64::from(area.h),
                }),
            );
        } else {
            window.window.set_ime_allowed(false);
        }
        if self.semantics_active
            && let Some(adapter) = &mut self.accessibility
        {
            adapter.update_if_active(|| self.semantics.update());
        }

        let action = if matches!(action, GreeterAction::Cancel) {
            action
        } else if self.pending {
            GreeterAction::None
        } else if action == GreeterAction::None
            && matches!(&self.flow, LoginFlow::Authenticating { prompt, .. } if !prompt.needs_response)
        {
            // The PAM instructions have now reached the screen. Advancing may block in fprintd,
            // but only the worker waits; the UI continues drawing and processing Escape.
            GreeterAction::SubmitResponse(None)
        } else {
            action
        };
        let acted = action != GreeterAction::None;
        if acted {
            self.apply_action(action);
        }
        if (acted || had_keys)
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
        if self.exit_code.is_some() {
            event_loop.exit();
        }
    }

    fn apply_action(&mut self, action: GreeterAction) {
        if self.cancel_pending || action == GreeterAction::None {
            return;
        }
        if self.pending {
            if action == GreeterAction::Cancel {
                self.cancel_pending = true;
                self.screen.clear_response();
                // Queue cancellation; never start a session from an intervening Ready update.
                let _ = self.commands.send(AuthCommand::Action(action));
            }
            return;
        }
        self.pending = self.commands.send(AuthCommand::Action(action)).is_ok();
        self.screen.clear_response();
    }

    /// Once `LoginFlow::ReadyToStart`, there is nothing left to ask the user — start the selected
    /// session immediately, the way `greetd-ipc(7)`'s flow expects, rather than waiting for a
    /// separate confirmation the protocol has no room for.
    fn maybe_start_session(&mut self) {
        let LoginFlow::ReadyToStart { .. } = &self.flow else {
            return;
        };
        let Some(entry) = self.sessions.get(self.screen.selected_session).cloned() else {
            self.flow = LoginFlow::Idle {
                error: Some("no session selected to start".to_owned()),
            };
            return;
        };
        // `create_session`/`post_auth_message_response` answered by PAM say nothing about display
        // type; only the greeter, which just read this entry out of `wayland-sessions/` or
        // `xsessions/`, knows which one is about to run.
        let env = match entry.kind {
            SessionKind::Wayland => vec!["XDG_SESSION_TYPE=wayland".to_owned()],
            SessionKind::X11 => vec!["XDG_SESSION_TYPE=x11".to_owned()],
        };
        self.pending = self
            .commands
            .send(AuthCommand::Start {
                cmd: entry.exec,
                env,
            })
            .is_ok();
    }

    fn handle_input_event(&mut self, event: WindowEvent) {
        let Some(window) = &mut self.window else {
            return;
        };
        match event {
            WindowEvent::Focused(focused) => {
                self.native_focused = focused;
                if !focused {
                    window.input.blur();
                    self.semantics.invalidate();
                }
                window.request_redraw();
            }
            WindowEvent::ThemeChanged(theme) => {
                self.theme = templar_ui::theme_for(&if theme == winit::window::Theme::Dark {
                    templar_types::appearance::DARK
                } else {
                    templar_types::appearance::LIGHT
                });
                window.request_redraw();
            }
            WindowEvent::Ime(ime) => {
                if self.pending || !self.native_focused {
                    return;
                }
                match ime {
                    winit::event::Ime::Preedit(text, _) => window.input.set_composition(&text),
                    winit::event::Ime::Commit(text) => {
                        window.input.set_composition("");
                        for character in text.chars() {
                            window
                                .input
                                .key(pixelkit_shell::KeyInput::Character(character));
                        }
                    }
                    winit::event::Ime::Disabled => window.input.set_composition(""),
                    winit::event::Ime::Enabled => {}
                }
                window.request_redraw();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                window.input.clear_accessible_text_edit();
                self.semantics.invalidate();
                if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                    window.scale = pixelkit_shell::Scale::new(scale_factor);
                }
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !self.native_focused {
                    return;
                }
                if !key_pressed(event.state) {
                    return;
                }
                let keys = translate_keys(&event.logical_key, event.text.as_deref());
                if !keys.is_empty() {
                    for key in keys {
                        window.input.key(key);
                    }
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                window.input.modifiers_changed(modifiers_of(modifiers));
            }
            WindowEvent::CursorMoved { position, .. } => {
                window
                    .input
                    .cursor_moved(position.x as f32, position.y as f32);
                window.request_redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                window.input.cursor_left();
                window.request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = translate_button(button) {
                    window.input.mouse(button, state == ElementState::Pressed);
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = scroll_pixels(delta);
                window.input.clear_accessible_text_edit();
                self.semantics.invalidate();
                window.input.scrolled(dx, dy);
                window.request_redraw();
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<AuthUpdate> for GreeterApp {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AuthUpdate) {
        match event {
            AuthUpdate::Wake => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AuthUpdate::Accessibility(event) => {
                if self
                    .window
                    .as_ref()
                    .is_none_or(|window| window.window.id() != event.window_id)
                {
                    return;
                }
                match event.window_event {
                    accesskit_winit::WindowEvent::InitialTreeRequested => {
                        self.semantics_active = true;
                        if let Some(adapter) = &mut self.accessibility {
                            adapter.update_if_active(|| self.semantics.update());
                        }
                    }
                    accesskit_winit::WindowEvent::ActionRequested(request) => {
                        if !self.pending
                            && let Some(window) = &mut self.window
                        {
                            self.semantics.action(&mut window.input, request);
                        }
                    }
                    accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                        self.semantics_active = false;
                        if let Some(window) = &mut self.window {
                            window.input.clear_accessible_text_edit();
                        }
                        self.semantics.invalidate();
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AuthUpdate::Started(username) => {
                if let Err(error) =
                    crate::last_user::save(std::path::Path::new(crate::last_user::PATH), &username)
                {
                    tracing::warn!(%error, "could not remember successful login username");
                }
                self.exit_code = Some(0);
                event_loop.exit();
            }
            AuthUpdate::Flow { flow, canceled } => {
                self.semantics.invalidate();
                if let Some(window) = &mut self.window {
                    window.input.blur();
                }
                self.flow = flow;
                if self.cancel_pending {
                    if canceled {
                        self.cancel_pending = false;
                        self.pending = false;
                    }
                } else {
                    self.pending = false;
                    self.maybe_start_session();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("TemplarOS Login")
                .with_fullscreen(Some(Fullscreen::Borderless(None))),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                tracing::error!(%error, "creating the greeter window");
                self.exit_code = Some(1);
                event_loop.exit();
                return;
            }
        };
        self.accessibility = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));
        self.window = match HostedWindow::new(window) {
            Ok(mut hosted) => {
                let proxy = self.proxy.clone();
                hosted
                    .input
                    .attach_clipboard(pixelkit_shell::clipboard::ClipboardWorker::new(move || {
                        let _ = proxy.send_event(AuthUpdate::Wake);
                    }));
                Some(hosted)
            }
            Err(error) => {
                tracing::error!(%error, "creating the greeter's presenter");
                self.exit_code = Some(1);
                event_loop.exit();
                return;
            }
        };
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WinitWindowId,
        event: WindowEvent,
    ) {
        let is_ours = self
            .window
            .as_ref()
            .is_some_and(|window| window.window.id() == window_id);
        if !is_ours {
            return;
        }
        if let (Some(adapter), Some(window)) = (&mut self.accessibility, &self.window) {
            adapter.process_event(&window.window, &event);
        }

        match event {
            WindowEvent::CloseRequested => {
                self.exit_code.get_or_insert(1);
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw(event_loop);
                return;
            }
            _ => {}
        }

        if self.pending
            && let WindowEvent::KeyboardInput { event, .. } = &event
        {
            if key_pressed(event.state)
                && event.logical_key
                    == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)
            {
                self.apply_action(GreeterAction::Cancel);
            }
            return;
        }
        self.handle_input_event(event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit_code.is_some() {
            event_loop.exit();
        }
    }
}
