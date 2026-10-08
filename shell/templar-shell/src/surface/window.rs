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
//!    to reach `templar_shell_v2` — there is no other way to a `wl_surface`/`wl_display` pointer, and
//!    `run_app` never exposes one.
//!
//! That is not fixable by a caller — it is a private field of a type this crate does not own, and
//! the fix is "don't use `run_app`", not a workaround inside it. The reusable native presenter
//! lives in `pixelkit-windowing`; `pixelkit-shell` retains the toolkit-neutral [`Input`],
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
use pixelkit_shell::{Input, KeyInput, Modifiers, MouseButton, Presenter, Scale};
use pixelkit_windowing::SoftbufferPresenter;
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

/// One window's own presenter, buffer, scale and input — everything
/// `pixelkit_shell::shell::Shell` keeps for its single window, reimplemented here so it can be
/// held once per window instead of at most once per process. See the module doc.
pub struct HostedWindow {
    pub window: Arc<Window>,
    /// Absent for Home on Wayland: its only presenter is the bridge's release-safe ARGB pool.
    presenter: Option<Box<dyn Presenter>>,
    pub buffer: WindowBuffer,
    pub scale: Scale,
    pub input: Input,
    ime_area: Option<pixelkit_raster::Rect>,
    pub semantics: pixelkit_ui::semantics::Semantics,
    accessibility: Option<accesskit_winit::Adapter>,
    semantic_epoch: u64,
    pub focus: pixelkit_ui::Focus,
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
            presenter: Some(Box::new(presenter)),
            buffer: WindowBuffer::new(1, 1),
            scale,
            input: Input::new(),
            ime_area: None,
            semantics: pixelkit_ui::semantics::Semantics::default(),
            accessibility: None,
            semantic_epoch: 0,
            focus: pixelkit_ui::Focus::new(),
            pending: false,
        })
    }

    /// Home uses the existing Wayland presenter, without allocating a softbuffer surface too.
    pub fn new_wayland(window: Arc<Window>) -> HostedWindow {
        let scale = Scale::new(window.scale_factor());
        HostedWindow {
            window,
            presenter: None,
            buffer: WindowBuffer::new(1, 1),
            scale,
            input: Input::new(),
            ime_area: None,
            semantics: pixelkit_ui::semantics::Semantics::default(),
            accessibility: None,
            semantic_epoch: 0,
            focus: pixelkit_ui::Focus::new(),
            pending: false,
        }
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }
    pub fn attach_accessibility(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        proxy: winit::event_loop::EventLoopProxy<super::app::Wake>,
    ) {
        self.accessibility = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &self.window,
            proxy,
        ));
    }

    pub fn accessibility_event(&mut self, event: accesskit_winit::WindowEvent) {
        match event {
            accesskit_winit::WindowEvent::InitialTreeRequested => self.publish_semantics(),
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                self.focus.clear();
                self.semantics.action(&mut self.input, request);
                self.retire_semantics();
                self.request_redraw();
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => self.invalidate_semantics(),
        }
    }

    pub fn process_accessibility_event(&mut self, event: &winit::event::WindowEvent) {
        if let Some(adapter) = &mut self.accessibility {
            adapter.process_event(&self.window, event);
        }
    }

    pub fn invalidate_semantics(&mut self) {
        self.input.clear_accessible_text_edit();
        self.retire_semantics();
    }

    fn retire_semantics(&mut self) {
        self.semantic_epoch = self
            .semantic_epoch
            .checked_add(1)
            .expect("Home semantic identity exhausted");
        self.semantics.invalidate();
    }

    pub fn begin_semantics(&mut self, title: &str) {
        self.semantics.begin_scoped(title, self.semantic_epoch);
    }

    pub fn publish_semantics(&mut self) {
        if let Some(adapter) = &mut self.accessibility {
            adapter.update_if_active(|| self.semantics.update());
        }
    }
    pub fn update_ime(&mut self, area: Option<pixelkit_raster::Rect>) {
        let area = area.filter(|_| self.window.has_focus());
        if self.ime_area == area {
            return;
        }
        if self.ime_area.is_some() != area.is_some() {
            self.window.set_ime_allowed(area.is_some());
        }
        if let Some(area) = area {
            self.window.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(area.x, area.y),
                winit::dpi::PhysicalSize::new(area.w.max(1), area.h.max(1)),
            );
        } else {
            self.input.set_composition("");
        }
        self.ime_area = area;
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
            if let Some(presenter) = &mut self.presenter
                && presenter.resize(size.width, size.height).is_err()
            {
                return false;
            }
        }
        true
    }

    pub fn present(&mut self) {
        if let Some(presenter) = &mut self.presenter {
            let _ = presenter.present(&self.buffer);
        }
    }
}

/// Paint after the modal owner has handled input in its input-first pass.
pub fn paint_modal_button(
    ui: &mut pixelkit_ui::Ui<'_>,
    area: pixelkit_raster::Rect,
    label: &str,
    focused: bool,
) {
    ui.semantic(
        pixelkit_ui::semantics::Role::Button,
        label,
        area,
        focused,
        None,
        None,
    );
    ui.painter.rounded_rect(
        area,
        ui.theme.corner_radius,
        if focused {
            ui.px(ui.theme.focus_width)
        } else {
            ui.theme.border_width
        },
        if ui.input.hovering(area) {
            ui.theme.hover
        } else {
            ui.theme.panel
        },
        if focused {
            ui.theme.accent
        } else {
            ui.theme.panel_edge
        },
    );
    ui.label(area, label, ui.theme.text, pixelkit_text::Align::Centre);
}

/// Existing focus/input for a custom-painted Home control; no parallel action model.
pub fn custom_control(
    ui: &mut pixelkit_ui::Ui<'_>,
    area: pixelkit_raster::Rect,
    label: &str,
) -> bool {
    let focused = ui.control_focus(area, ui.input.focus_requested(area));
    ui.semantic(
        pixelkit_ui::semantics::Role::Button,
        label,
        area,
        focused,
        None,
        None,
    );
    if focused {
        super::design::focus_ring(ui, area);
    }
    ui.input.take_click(area)
        || (focused
            && ui.input.consume_keys(|keys| {
                keys.iter()
                    .any(|key| matches!(key, KeyInput::Enter | KeyInput::Character(' ')))
            }))
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
