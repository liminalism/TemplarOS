//! Wiring: one `winit::application::ApplicationHandler` hosting the greeter's one full-screen
//! window, `crate::login::LoginController` driving `crate::client::GreetdClient` for it, and
//! `crate::screen::GreeterScreen` turning key/mouse input into `crate::login`/`crate::sessions`
//! actions. See `surface::window`'s module doc for why this crate drives winit directly rather than
//! `pixelkit_shell::run_app` — the fullscreen requirement `WindowConfig` has no way to express.
//!
//! No async runtime, no background thread, no `Waker`: `GreetdClient`'s calls block
//! (`crate::client`'s module doc), so everything here happens synchronously in response to a
//! window event, the same way a native blocking login prompt behaves — including a brief freeze
//! during PAM's own fail-delay on a wrong password, which is expected, not a bug.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use anyhow::Context;
use pixelkit_raster::{Painter, RasterKernel, Rect};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
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

    let event_loop = EventLoop::new().context("creating the winit event loop")?;
    let mut app = GreeterApp::new(client, sessions, selected_session);
    event_loop
        .run_app(&mut app)
        .context("running the greeter's event loop")?;
    Ok(app.exit_code.unwrap_or(1))
}

struct GreeterApp {
    controller: LoginController<UnixStream>,
    sessions: Vec<SessionEntry>,
    window: Option<HostedWindow>,
    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    theme: Theme,
    screen: GreeterScreen,
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
    ) -> GreeterApp {
        let screen = GreeterScreen {
            selected_session,
            ..GreeterScreen::default()
        };
        GreeterApp {
            controller: LoginController::new(client),
            sessions,
            window: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            theme: Theme::default(),
            screen,
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
            flow: &self.controller.flow,
            sessions: &self.sessions,
        };
        let action = {
            let painter = Painter::new(&mut window.buffer);
            let mut ui = Ui::new(
                painter,
                &mut self.text,
                &mut window.input,
                self.theme,
                scale,
                &mut self.kernel,
            );
            let area = Rect::new(0, 0, width, height);
            let action = self.screen.draw(&mut ui, area, &data);
            // The session picker's open popup, if any, draws last — `Dropdown::draw`'s own
            // contract (`pixelkit-ui/src/dropdown.rs`).
            self.screen.draw_session_popup(&mut ui, area);
            action
        };
        window.input.end_frame();
        window.present();

        let acted = action != GreeterAction::None;
        if acted {
            self.apply_action(action);
            self.maybe_start_session();
        }
        if acted || had_keys {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if self.exit_code.is_some() {
            event_loop.exit();
        }
    }

    fn apply_action(&mut self, action: GreeterAction) {
        match action {
            GreeterAction::None => {}
            GreeterAction::SubmitUsername(username) => self.controller.submit_username(username),
            GreeterAction::SubmitResponse(text) => self.controller.submit_response(text),
            GreeterAction::Cancel => self.controller.cancel(),
        }
        // Nothing typed for a prompt survives past the exchange it was an answer to — an error,
        // a cancel, and a session actually starting all leave `Authenticating` behind for good.
        if !matches!(self.controller.flow, LoginFlow::Authenticating { .. }) {
            self.screen.clear_response();
        }
    }

    /// Once `LoginFlow::ReadyToStart`, there is nothing left to ask the user — start the selected
    /// session immediately, the way `greetd-ipc(7)`'s flow expects, rather than waiting for a
    /// separate confirmation the protocol has no room for.
    fn maybe_start_session(&mut self) {
        let LoginFlow::ReadyToStart { .. } = &self.controller.flow else {
            return;
        };
        let Some(entry) = self.sessions.get(self.screen.selected_session).cloned() else {
            self.controller.flow = LoginFlow::Idle {
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
        if self.controller.start_session(entry.exec, env) {
            tracing::info!(
                session = %entry.name,
                "session started; exiting so greetd can hand off the VT (greetd(5))"
            );
            self.exit_code = Some(0);
        } else {
            self.screen.clear_response();
        }
    }
}

impl ApplicationHandler for GreeterApp {
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
        self.window = match HostedWindow::new(window) {
            Ok(hosted) => Some(hosted),
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

        let Some(window) = &mut self.window else {
            return;
        };
        match event {
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                    window.scale = pixelkit_shell::Scale::new(scale_factor);
                }
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
                window.input.scrolled(dx, dy);
                window.request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit_code.is_some() {
            event_loop.exit();
        }
    }
}
