//! Explicitly opt-in, nested-only test input. No Wayland global, uinput device, or host input.
//! A private Unix datagram socket drives the *same* InputEvent dispatch as physical backends.
use crate::state::{Backend, TemplarCompState};
use smithay::backend::input::{
    self, AbsolutePositionEvent, Device, DeviceCapability, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerButtonEvent, PointerMotionAbsoluteEvent, UnusedEvent,
};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use smithay::reexports::wayland_server::Resource;
use std::{
    collections::HashSet,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::Path,
};

struct WallpaperFixture {
    image: templar_types::wallpaper::WallpaperImage,
    mapping: Result<templar_wallpaper::raster::RasterMapping, String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TestDevice;
impl Device for TestDevice {
    fn id(&self) -> String {
        "nested-test".into()
    }
    fn name(&self) -> String {
        "Private nested test input".into()
    }
    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(
            capability,
            DeviceCapability::Keyboard | DeviceCapability::Pointer
        )
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<std::path::PathBuf> {
        None
    }
}
#[derive(Debug)]
pub struct TestBackend;
impl InputBackend for TestBackend {
    type Device = TestDevice;
    type KeyboardKeyEvent = TestEvent;
    type PointerButtonEvent = TestEvent;
    type PointerMotionAbsoluteEvent = TestEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}
#[derive(Debug)]
pub struct TestEvent {
    code: u32,
    pressed: bool,
    x: f64,
    y: f64,
    time: u64,
    count: u32,
}
impl Event<TestBackend> for TestEvent {
    fn time(&self) -> u64 {
        self.time
    }
    fn device(&self) -> TestDevice {
        TestDevice
    }
}
impl KeyboardKeyEvent<TestBackend> for TestEvent {
    fn key_code(&self) -> input::Keycode {
        (self.code + 8).into()
    }
    fn state(&self) -> input::KeyState {
        if self.pressed {
            input::KeyState::Pressed
        } else {
            input::KeyState::Released
        }
    }
    fn count(&self) -> u32 {
        self.count
    }
}
impl PointerButtonEvent<TestBackend> for TestEvent {
    fn button_code(&self) -> u32 {
        self.code
    }
    fn state(&self) -> input::ButtonState {
        if self.pressed {
            input::ButtonState::Pressed
        } else {
            input::ButtonState::Released
        }
    }
}
impl AbsolutePositionEvent<TestBackend> for TestEvent {
    fn x(&self) -> f64 {
        self.x
    }
    fn y(&self) -> f64 {
        self.y
    }
    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }
    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}
impl PointerMotionAbsoluteEvent<TestBackend> for TestEvent {}

#[derive(Debug)]
pub struct SocketGuard(std::path::PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn install<B: Backend + 'static>(
    handle: &LoopHandle<'static, TemplarCompState<B>>,
    path: &Path,
) -> anyhow::Result<SocketGuard> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("input socket requires a private parent directory"))?;
    let metadata = std::fs::metadata(parent)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode().trailing_zeros() >= 6,
        "input socket parent must be private (0700)"
    );
    // Never unlink an existing socket: another test/session may own it.
    let socket = UnixDatagram::bind(path)?;
    let guard = SocketGuard(path.to_path_buf());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    let mut held = HashSet::new();
    let (wallpaper_sender, wallpaper_receiver) =
        smithay::reexports::calloop::channel::channel::<WallpaperFixture>();
    let wallpaper_busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completed = wallpaper_busy.clone();
    handle
        .insert_source(wallpaper_receiver, move |event, (), state| {
            if let smithay::reexports::calloop::channel::Event::Msg(fixture) = event {
                completed.store(false, std::sync::atomic::Ordering::Release);
                let output = fixture.image.output.clone();
                let result = if fixture.image.display_revision != state.display_control.revision() {
                    Err("Test wallpaper topology changed during mapping".into())
                } else {
                    fixture
                        .mapping
                        .and_then(|mapping| state.wallpaper.accept(fixture.image, mapping))
                };
                if let Err(failure) = result {
                    state.wallpaper.reject(&output, failure);
                }
                B::request_redraw(state);
            }
        })
        .map_err(|error| anyhow::anyhow!("installing test wallpaper import: {error}"))?;
    handle
        .insert_source(
            Generic::new(socket, Interest::READ, Mode::Level),
            move |_, socket, state| {
                let mut buffer = [0u8; 256];
                loop {
                    let (length, peer) = match socket.recv_from(&mut buffer) {
                        Ok(value) => value,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    };
                    let response = match std::str::from_utf8(&buffer[..length]) {
                        Ok(command) => execute(
                            state,
                            command,
                            &mut held,
                            &wallpaper_sender,
                            &wallpaper_busy,
                        ),
                        Err(_) => Err(anyhow::anyhow!("command is not UTF-8")),
                    };
                    if let Some(peer) = peer.as_pathname() {
                        let response = response.unwrap_or_else(
                            |error| serde_json::json!({"error": error.to_string()}),
                        );
                        let _ = socket.send_to(response.to_string().as_bytes(), peer);
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| anyhow::anyhow!("installing private input: {error}"))?;
    tracing::info!(path = %path.display(), "private nested test input ready");
    Ok(guard)
}

fn execute<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    command: &str,
    held: &mut HashSet<u32>,
    wallpaper_sender: &smithay::reexports::calloop::channel::Sender<WallpaperFixture>,
    wallpaper_busy: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<serde_json::Value> {
    let mut words = command.split_whitespace();
    let args = [words.next(), words.next(), words.next(), words.next()];
    let time = crate::window_bridge::now_monotonic_ns(&state.clock) / 1000;
    let mut event = TestEvent {
        code: 0,
        pressed: false,
        x: 0.0,
        y: 0.0,
        time,
        count: 0,
    };
    match args {
        [Some("preview-snapshot"), None, None, None] => {
            return Ok(serde_json::json!({
                "rendered": state.screencopy.rendered_previews,
                "pending": state.screencopy.pending.len(),
                "demanded": state.screencopy.demand.as_ref().map_or(0, |demand| demand.len()),
                "generation": state.screencopy.demand.as_ref().map(|demand| demand.generation()),
            }));
        }
        [Some("wallpaper-snapshot"), None, None, None] => {
            return Ok(serde_json::to_value(state.wallpaper.observation())?);
        }
        [Some("wallpaper-clear"), None, None, None] => {
            let revision = state.wallpaper.observation().revision;
            state
                .wallpaper
                .clear(revision, None)
                .map_err(anyhow::Error::msg)?;
            B::request_redraw(state);
            return Ok(serde_json::to_value(state.wallpaper.observation())?);
        }
        [
            Some("wallpaper-fixture"),
            Some(sizing),
            Some(format),
            Some(source),
        ] => {
            anyhow::ensure!(
                !state.session_lock.locked && !state.idle.blanked(),
                "fixture requires an unlocked lit output"
            );
            let sizing = serde_json::from_value(serde_json::Value::String(sizing.into()))?;
            let format = match format {
                "rgba" => templar_types::wallpaper::PixelFormat::Rgba8,
                "indexed" => templar_types::wallpaper::PixelFormat::Indexed8,
                _ => anyhow::bail!("fixture format must be rgba or indexed"),
            };
            let (width, height) = source
                .split_once('x')
                .ok_or_else(|| anyhow::anyhow!("source must be WIDTHxHEIGHT"))?;
            let (source_width, source_height) = (width.parse::<u32>()?, height.parse::<u32>()?);
            anyhow::ensure!(
                source_width > 0 && source_height > 0,
                "empty source dimensions"
            );
            let output = state
                .space
                .outputs()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no nested output"))?
                .name();
            let revision = state.wallpaper.observation().revision;
            let palette = vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 255, 255],
            ];
            let indexed = format == templar_types::wallpaper::PixelFormat::Indexed8;
            let image = templar_types::wallpaper::WallpaperImage {
                revision: revision.at_generation(revision.generation + 1),
                display_revision: state.display_control.revision(),
                output: templar_types::display::DisplayId(output),
                sizing,
                ground: [24, 26, 30, 255],
                transition_budget_bytes: 16 * 1024 * 1024,
                raster: templar_types::wallpaper::RasterDescriptor {
                    version: 1,
                    width: 2,
                    height: 2,
                    source_width,
                    source_height,
                    stride: if indexed { 2 } else { 8 },
                    format,
                    palette: if indexed { palette.clone() } else { Vec::new() },
                    bytes: if indexed { 4 } else { 16 },
                },
            };
            anyhow::ensure!(
                wallpaper_busy
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire
                    )
                    .is_ok(),
                "one wallpaper fixture is already mapping"
            );
            let sender = wallpaper_sender.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let mapping = (|| -> anyhow::Result<_> {
                    let mut file = templar_wallpaper::raster::create_memfd()?;
                    if indexed {
                        file.write_all(&[0, 1, 2, 3])?;
                    } else {
                        for color in palette {
                            file.write_all(&color)?;
                        }
                    }
                    templar_wallpaper::raster::seal(&file)?;
                    Ok(templar_wallpaper::raster::RasterMapping::new(
                        &file,
                        &image.raster,
                    )?)
                })()
                .map_err(|error| error.to_string());
                let _ = sender.send(WallpaperFixture { image, mapping });
            });
            return Ok(serde_json::json!({"pending": true}));
        }
        [Some("snapshot"), None, None, None] => {
            let windows: Vec<_> = state.window_service.windows().filter(|window| state.window_service.is_client_window(window.id) && window.mapped).map(|window| {
                let card = state.motion.target(window.id).and_then(|target| {
                    let surface = state.window_service.window(target.surface_window)?;
                    Some(serde_json::json!({"x": surface.geometry.x + target.rect.x, "y": surface.geometry.y + target.rect.y, "width": target.rect.width, "height": target.rect.height}))
                });
                serde_json::json!({"id": window.id.0, "title": window.title, "active": window.activated, "parked": window.shelved, "card": card,
                    "transient_parent": window.transient_parent.map(|id| id.0), "fullscreen": window.fullscreen,
                    "geometry": window.geometry, "peer_pid": window.peer_pid, "identity_verified": window.identity.verified,
                    "layer": format!("{:?}", state.window_service.layer_of(window.id)),
                    "stacking_preference": format!("{:?}", window.stacking),
                    "tab": tab_snapshot(&state.window_service, window.id)})
            }).collect();
            let pointer = state.seat.get_pointer().map(|pointer| {
                let position = pointer.current_location();
                serde_json::json!({"x": position.x, "y": position.y,
                    "window": pointer.current_focus().and_then(|surface| state.windows.window_id_for(&surface.id())).map(|id| id.0)})
            });
            // Whether a shell `Layer::Presentation` surface (Taskfield labels, the shell-drawn
            // switcher) is actually in the render mirror — mapped is not the same as drawn.
            let presentation_in_space = state.window_service.windows().any(|window| {
                state.window_service.layer_of(window.id)
                    == Some(templar_window::Layer::Presentation)
                    && state
                        .windows
                        .element_of(window.id)
                        .is_some_and(|element| state.space.element_location(element).is_some())
            });
            return Ok(
                serde_json::json!({"windows": windows, "pointer": pointer, "stacking": state.window_service.stacking_order().iter().map(|id| id.0).collect::<Vec<_>>(), "home": state.window_service.home_revealed(), "selected": state.switcher.as_ref().map(|switcher| switcher.selected.0), "presentation_in_space": presentation_in_space,
                "name_tab_menu": state.name_tab_menu.open.map(|menu| serde_json::json!({
                    "window": menu.window_id.0, "panel": menu.panel,
                    "rows": crate::chrome::NameTabMenuTarget::ROWS.iter().enumerate().map(|(index, (choice, label))|
                        serde_json::json!({"choice": format!("{choice:?}"), "label": label, "rect": menu.row(index)})).collect::<Vec<_>>()
                }))}),
            );
        }
        [
            Some(kind @ ("key" | "button")),
            Some(code),
            Some(direction @ ("down" | "up")),
            None,
        ] => {
            event.code = code.parse()?;
            anyhow::ensure!(event.code <= 767, "invalid evdev code");
            event.pressed = direction == "down";
            if kind == "key" {
                if event.pressed {
                    held.insert(event.code);
                } else {
                    held.remove(&event.code);
                }
                event.count = u32::try_from(held.len())?;
                state.process_input_event::<TestBackend>(
                    InputEvent::Keyboard { event },
                    Some("winit"),
                );
            } else {
                state.process_input_event::<TestBackend>(
                    InputEvent::PointerButton { event },
                    Some("winit"),
                );
            }
        }
        [Some("motion"), Some(x), Some(y), None] => {
            event.x = x.parse()?;
            event.y = y.parse()?;
            anyhow::ensure!(
                event.x.is_finite() && event.y.is_finite(),
                "non-finite pointer coordinate"
            );
            // The control socket accepts physical canvas pixels; production absolute producers
            // expose unit coordinates before the viewport maps/clips them into logical space.
            let mode = state
                .space
                .outputs()
                .next()
                .and_then(|output| output.current_mode())
                .ok_or_else(|| anyhow::anyhow!("nested output has no native mode"))?;
            anyhow::ensure!(mode.size.w > 0 && mode.size.h > 0, "empty nested canvas");
            event.x /= f64::from(mode.size.w);
            event.y /= f64::from(mode.size.h);
            state.process_input_event::<TestBackend>(
                InputEvent::PointerMotionAbsolute { event },
                Some("winit"),
            );
        }
        _ => anyhow::bail!("expected snapshot | key/button EVDEV down/up | motion X Y"),
    }
    Ok(serde_json::json!({"ok": true}))
}

/// One window's tab state for the `snapshot` fixture: the active corner, the aim latch, what
/// is drawn, and the title rectangle — what a nested tab test asserts corner-following,
/// reveal and latch behaviour against.
pub fn tab_snapshot<C: templar_window::IdentityCache>(
    window_service: &templar_window::WindowService<C>,
    window_id: templar_window::WindowId,
) -> serde_json::Value {
    let layout = window_service.window_layout(window_id);
    serde_json::json!({
        "corner": window_service.tab_corner(window_id).name(),
        "latched": window_service.is_tab_latched(window_id),
        "title_revealed": window_service.title_revealed(window_id),
        "controls_revealed": window_service.controls_revealed(window_id),
        "overlay": layout.is_some_and(|layout| layout.overlay),
        "title": layout.map(|layout| layout.title),
        "controls": layout.map(|layout| layout.controls),
        "resize_squares": layout.map(|layout| templar_window::WindowLayout::resize_squares(layout.client)),
        "activation_regions": layout.map(|layout| templar_window::WindowLayout::activation_regions(layout.client)),
        "input_bounds": layout.map(|layout| layout.input_bounds()),
        "damage_bounds": layout.map(|layout| layout.damage_bounds()),
        "pending": window_service.has_tab_pending(window_id),
        "next_deadline_ns": window_service.next_tab_deadline(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_use_evdev_keycodes_and_absolute_output_coordinates() {
        let event = TestEvent {
            code: 56,
            pressed: true,
            x: 123.0 / 1920.0,
            y: 456.0 / 1080.0,
            time: 9,
            count: 1,
        };
        assert_eq!(
            KeyboardKeyEvent::<TestBackend>::key_code(&event),
            input::Keycode::from(64u32)
        );
        assert_eq!(
            KeyboardKeyEvent::<TestBackend>::state(&event),
            input::KeyState::Pressed
        );
        assert!((event.x_transformed(1920) - 123.0).abs() <= f64::EPSILON * 1920.0);
        assert!((event.y_transformed(1080) - 456.0).abs() <= f64::EPSILON * 1080.0);
        assert!((event.x_transformed(1) - 123.0 / 1920.0).abs() < f64::EPSILON);
        assert!((event.y_transformed(1) - 456.0 / 1080.0).abs() < f64::EPSILON);
    }

    /// The `tab` fixture object reports the corner, latch, reveal and title rect a
    /// nested test needs: follow moves the corner, reveal flips the flags, overlay shows.
    #[test]
    fn tab_snapshot_reports_corner_latch_reveal_and_overlay() {
        use templar_window::{ClientKind, Corner, Output, Rect, WindowService};
        struct NullCache;
        impl templar_window::IdentityCache for NullCache {
            fn resolve_cached(
                &self,
                _pid: u32,
                _claims: &templar_types::WindowClaims,
            ) -> Option<templar_window::CachedIdentity> {
                None
            }
            fn request_resolution(
                &self,
                _window_id: templar_window::WindowId,
                _pid: u32,
                _claims: templar_types::WindowClaims,
            ) {
            }
        }
        let outputs = vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)];
        let mut service = WindowService::new(NullCache, outputs);
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            templar_types::WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        let shot = tab_snapshot(&service, id);
        assert_eq!(shot["corner"], "top-right");
        assert_eq!(shot["latched"], false);
        assert_eq!(shot["title_revealed"], true);
        assert_eq!(shot["controls_revealed"], false);
        assert_eq!(shot["overlay"], false);
        assert!(shot["title"].is_object());

        service.set_tab_corner(id, Corner::BottomLeft);
        service.set_tab_latched(id, true);
        service.set_controls_revealed(id, true);
        service.set_maximized(id, true);
        let shot = tab_snapshot(&service, id);
        assert_eq!(shot["corner"], "bottom-left");
        assert_eq!(shot["latched"], true);
        assert_eq!(shot["controls_revealed"], true);
        assert_eq!(shot["overlay"], true);
    }

    #[test]
    fn input_endpoint_refuses_public_directory_and_never_unlinks_existing_socket() {
        use crate::backend::winit::WinitBackendData;
        let event_loop =
            smithay::reexports::calloop::EventLoop::<TemplarCompState<WinitBackendData>>::try_new()
                .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.sock");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(install(&event_loop.handle(), &path).is_err());
        assert!(!path.exists());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing = UnixDatagram::bind(&path).unwrap();
        assert!(install(&event_loop.handle(), &path).is_err());
        assert!(path.exists());
        drop(existing);
    }
}
