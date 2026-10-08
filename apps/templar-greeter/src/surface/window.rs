//! One hosted window: presenter, buffer, scale and input — the same shape
//! `shell/templar-shell/src/surface/window.rs`'s `HostedWindow` has, and largely the same code,
//! copied rather than shared for the same reason `surface/fonts.rs` copies its font: crates in
//! this workspace do not share private application code, only published crates like `pixelkit`.
//!
//! What differs from the shell's reason for existing at all: `shell/templar-shell/src/surface/app.rs`
//! bypasses `pixelkit_shell::run_app` because it needs the window's *raw platform handle* (to reach
//! the compositor's private `templar_shell_v1` protocol) and *more than one window* (Home Surface +
//! dock) — neither of which this crate needs; a greeter is one window and one ordinary
//! `xdg_toplevel`, nothing private to reach. What this crate *does* need that `run_app`'s
//! `WindowConfig` has no way to ask for is **fullscreen**: `WindowConfig` (`pixelkit-shell/src/
//! shell.rs`) exposes only a logical width/height, no `winit::window::Fullscreen`. A login screen
//! that opens at some fixed logical size in the corner of whatever output greetd's VT ends up on is
//! not the "full-screen window" the task calls for, so this crate drives `winit::application::
//! ApplicationHandler` directly too — for a different, narrower reason than the shell's, but the
//! same unavailable-through-the-higher-level-API shape. Worth reporting upstream: `WindowConfig`
//! having no fullscreen option is a real pixelkit gap, distinct from the raw-handle/multi-window
//! ones `shell/templar-shell/src/surface/app.rs`'s own module doc already names.

use std::sync::Arc;

use pixelkit_raster::WindowBuffer;
use pixelkit_shell::{
    Input, KeyInput, Modifiers, MouseButton, Presenter, Scale,
};
use pixelkit_windowing::SoftbufferPresenter;
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

/// One window's own presenter, buffer, scale and input.
pub struct HostedWindow {
    pub window: Arc<Window>,
    presenter: Box<dyn Presenter>,
    pub buffer: WindowBuffer,
    pub scale: Scale,
    pub input: Input,
}

impl std::fmt::Debug for HostedWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedWindow")
            .field("window", &self.window.id())
            .field("size", &(self.buffer.width, self.buffer.height))
            .finish_non_exhaustive()
    }
}

impl HostedWindow {
    pub fn new(window: Arc<Window>) -> anyhow::Result<HostedWindow> {
        let presenter = SoftbufferPresenter::new(window.clone())
            .map_err(|error| anyhow::anyhow!("creating a software presenter: {error}"))?;
        let scale = Scale::new(window.scale_factor());
        Ok(HostedWindow {
            window,
            presenter: Box::new(presenter),
            buffer: WindowBuffer::new(1, 1),
            scale,
            input: Input::new(),
        })
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Resize the backing buffer/presenter to the window's current physical size. Returns `false`
    /// for a zero-sized window (between an initial configure and the first commit, or minimized) —
    /// nothing to paint into yet, so the caller should skip drawing this frame.
    pub fn prepare(&mut self) -> bool {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return false;
        }
        if self.buffer.width != size.width || self.buffer.height != size.height {
            self.buffer.resize(size.width, size.height);
            if self.presenter.resize(size.width, size.height).is_err() {
                return false;
            }
        }
        true
    }

    pub fn present(&mut self) {
        let _ = self.presenter.present(&self.buffer);
    }
}

/// Winit's logical key to this crate's input vocabulary — a copy of
/// `shell/templar-shell/src/surface/window.rs`'s identical function (itself a copy of
/// `pixelkit_shell::shell`'s private `translate`; see that file's doc for why it cannot be reused
/// instead of copied).
///
/// **A key press can be more than one character**, and here that matters more than anywhere else:
/// this is where a password is typed, and a password with an accent in it that silently loses the
/// accent is an unexplainable failed login. `Key::Character` carries a `SmolStr` because a dead
/// key or compose sequence with no precomposed form commits a base letter and a combining mark in
/// one event; every scalar is returned and the caller queues one [`KeyInput`] for each.
pub fn translate_keys(logical: &Key, text: Option<&str>) -> Vec<KeyInput> {
    if let Some(named) = translate_named(logical) {
        return vec![named];
    }
    let characters = match logical {
        Key::Character(characters) => Some(characters.as_str()),
        _ => text,
    };
    characters
        .unwrap_or_default()
        .chars()
        .map(KeyInput::Character)
        .collect()
}

/// The named keys, which are always exactly one [`KeyInput`].
fn translate_named(logical: &Key) -> Option<KeyInput> {
    match logical {
        Key::Named(NamedKey::Enter) => Some(KeyInput::Enter),
        Key::Named(NamedKey::Escape) => Some(KeyInput::Escape),
        Key::Named(NamedKey::Backspace) => Some(KeyInput::Backspace),
        Key::Named(NamedKey::Tab) => Some(KeyInput::Tab),
        Key::Named(NamedKey::Delete) => Some(KeyInput::Delete),
        Key::Named(NamedKey::Home) => Some(KeyInput::Home),
        Key::Named(NamedKey::End) => Some(KeyInput::End),
        Key::Named(NamedKey::PageUp) => Some(KeyInput::PageUp),
        Key::Named(NamedKey::PageDown) => Some(KeyInput::PageDown),
        Key::Named(NamedKey::ArrowLeft) => Some(KeyInput::Left),
        Key::Named(NamedKey::ArrowRight) => Some(KeyInput::Right),
        Key::Named(NamedKey::ArrowUp) => Some(KeyInput::Up),
        Key::Named(NamedKey::ArrowDown) => Some(KeyInput::Down),
        Key::Named(NamedKey::F1) => Some(KeyInput::Function(1)),
        Key::Named(NamedKey::F2) => Some(KeyInput::Function(2)),
        Key::Named(NamedKey::F3) => Some(KeyInput::Function(3)),
        Key::Named(NamedKey::F4) => Some(KeyInput::Function(4)),
        Key::Named(NamedKey::F5) => Some(KeyInput::Function(5)),
        Key::Named(NamedKey::F12) => Some(KeyInput::Function(12)),
        _ => None,
    }
}

pub fn translate_button(button: WinitButton) -> Option<MouseButton> {
    match button {
        WinitButton::Left => Some(MouseButton::Left),
        WinitButton::Right => Some(MouseButton::Right),
        WinitButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

const PIXELS_PER_LINE: f32 = 24.0;

pub fn scroll_pixels(delta: MouseScrollDelta) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (x * PIXELS_PER_LINE, y * PIXELS_PER_LINE),
        MouseScrollDelta::PixelDelta(position) => (position.x as f32, position.y as f32),
    }
}

pub fn key_pressed(state: ElementState) -> bool {
    state == ElementState::Pressed
}

pub fn modifiers_of(modifiers: winit::event::Modifiers) -> Modifiers {
    let state = modifiers.state();
    Modifiers {
        shift: state.shift_key(),
        control: state.control_key(),
        alt: state.alt_key(),
        logo: state.super_key(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    #[test]
    fn tab_translates_for_focus_cycling() {
        assert_eq!(
            translate_keys(&Key::Named(NamedKey::Tab), None),
            vec![KeyInput::Tab]
        );
    }

    #[test]
    fn escape_translates_rather_than_being_swallowed() {
        assert_eq!(
            translate_keys(&Key::Named(NamedKey::Escape), None),
            vec![KeyInput::Escape]
        );
    }

    #[test]
    fn a_character_key_translates() {
        assert_eq!(
            translate_keys(&Key::Character(SmolStr::new("q")), Some("q")),
            vec![KeyInput::Character('q')]
        );
    }

    /// A password may contain an accented character that the layout commits as two scalars.
    /// Dropping the second is a login that fails for no visible reason.
    #[test]
    fn a_multi_scalar_press_keeps_every_scalar() {
        assert_eq!(
            translate_keys(
                &Key::Character(SmolStr::new("e\u{0301}")),
                Some("e\u{0301}")
            ),
            vec![KeyInput::Character('e'), KeyInput::Character('\u{0301}')]
        );
    }

    #[test]
    fn a_press_that_commits_nothing_produces_nothing() {
        assert!(translate_keys(&Key::Named(NamedKey::Shift), None).is_empty());
    }

    #[test]
    fn mouse_buttons_translate_and_extras_are_ignored() {
        assert_eq!(translate_button(WinitButton::Left), Some(MouseButton::Left));
        assert_eq!(translate_button(WinitButton::Back), None);
    }

    #[test]
    fn scrolling_normalises_to_pixels() {
        let (_, y) = scroll_pixels(MouseScrollDelta::LineDelta(0.0, -1.0));
        assert!((y - -PIXELS_PER_LINE).abs() < f32::EPSILON);
    }
}
