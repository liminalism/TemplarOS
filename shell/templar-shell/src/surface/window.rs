//! Why this crate drives its own `winit::application::ApplicationHandler` instead of
//! `pixelkit_shell::run_app`/`PixelApp` — the design question the milestone brief calls out by
//! name.
//!
//! `pixelkit_shell::shell::Shell` (the type `run_app` builds and drives) owns exactly one
//! `winit::window::Window`, and never hands it — or the `ActiveEventLoop` that created it — to the
//! `PixelApp` it hosts; every `PixelApp` method (`render`, `attach`, `tick`, `on_key`, …) receives
//! only a pixel buffer, a [`Waker`](pixelkit_shell::Waker), or nothing at all. The thing this
//! crate needs is therefore unreachable through that API:
//!
//! 1. **The window's raw platform handle.** `crate::wayland_bridge::ShellBridge` has to call
//!    `raw_window_handle::HasWindowHandle`/`HasDisplayHandle` on the actual `winit::window::Window`
//!    to reach `templar_shell_v1` — there is no other way to a `wl_surface`/`wl_display` pointer, and
//!    `run_app` never exposes one.
//!
//! That is not fixable by a caller — it is a private field of a type this crate does not own, and
//! the fix is "don't use `run_app`", not a workaround inside it. What *is* reusable, because
//! `pixelkit-shell`'s README lists them as independently public pieces rather than baking them
//! into `Shell`, is everything below `run_app` in the stack: [`SoftbufferPresenter`], [`Input`],
//! [`Scale`], [`FrameTimer`], and the `KeyInput`/`Modifiers`/`MouseButton` vocabulary. This module
//! is the thin `ApplicationHandler` that wires those pieces to winit directly, hosting the Home
//! window and handing its `&Window` to whoever asks — the same shape
//! `pixelkit_shell::shell::Shell` has internally, just not private.
//!
//! One real, small gap this leaves: `Shell`'s winit-key-to-`KeyInput` translation (`translate`),
//! its mouse-button mapping and its wheel-delta normalisation are private module functions, not
//! part of the public surface the README advertises — so [`translate_keys`]/[`translate_button`]/
//! [`scroll_pixels`] below are copies, not reuse. They are small and mechanical (no logic beyond a
//! `match`), but a genuine gap worth reporting: nothing about needing raw window-handle access
//! explains why *this* part had to be duplicated too.

use std::sync::Arc;

use pixelkit_raster::WindowBuffer;
use pixelkit_shell::{
    Input, KeyInput, Modifiers, MouseButton, Presenter, Scale, SoftbufferPresenter,
};
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

/// One window's own presenter, buffer, scale and input — everything
/// `pixelkit_shell::shell::Shell` keeps for its single window, reimplemented here so it can be
/// held once per window instead of at most once per process. See the module doc.
pub struct HostedWindow {
    pub window: Arc<Window>,
    presenter: Box<dyn Presenter>,
    pub buffer: WindowBuffer,
    pub scale: Scale,
    pub input: Input,
    /// A masked present was deferred because every buffer was still held by the compositor
    /// (`wayland_bridge::PresentOutcome::Deferred`). The frame is still owed: the host redraws
    /// this window once the bridge reports a buffer released, whether or not any input arrives.
    pub pending: bool,
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
            pending: false,
        })
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Resize the backing buffer/presenter to the window's current physical size. Returns `false`
    /// for a zero-sized window (between an initial configure and the first commit, or
    /// minimized) — nothing to paint into yet, so the caller should skip drawing this frame.
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

/// Winit's logical key to this crate's input vocabulary. A copy of
/// `pixelkit_shell::shell`'s private `translate` — see the module doc.
///
/// **A key press can be more than one character.** `Key::Character` carries a `SmolStr`, not a
/// `char`, because one press can commit several scalars at once: a dead key or a compose sequence
/// whose result has no precomposed form arrives as a base letter followed by a combining mark, in
/// a single event. Keeping only the first of those silently drops the accent and leaves the field
/// with no way to know anything went missing — so this returns every scalar the press produced,
/// and the caller queues one [`KeyInput`] per scalar. `Input` holds `keys` as a `Vec` and a text
/// field consumes all of them in the frame that follows, so several in one press cost nothing.
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

/// The named keys, which are always exactly one [`KeyInput`]. Split out so [`translate_keys`] can
/// answer "was this a named key at all?" without duplicating the table.
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

    /// A dead-key or compose commit with no precomposed form arrives as several scalars in one
    /// press. Every one of them must reach the field: keeping only the first is how an accent
    /// disappears without anything reporting that it did.
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

    /// Some layouts report no logical key but do report committed text; that route must carry
    /// every scalar too, not just the first.
    #[test]
    fn text_without_a_character_key_keeps_every_scalar() {
        assert_eq!(
            translate_keys(&Key::Named(NamedKey::Alt), Some("a\u{0308}")),
            vec![KeyInput::Character('a'), KeyInput::Character('\u{0308}')]
        );
    }

    /// A modifier press on its own commits nothing — it must not produce a phantom character.
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
