//! Explicitly opt-in, nested-only test input. No Wayland global, uinput device, or host input.
//! A private Unix datagram socket drives the *same* InputEvent dispatch as physical backends.
use crate::state::{Backend, TemplarCompState};
use smithay::backend::input::{
    self, AbsolutePositionEvent, Device, DeviceCapability, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerButtonEvent, PointerMotionAbsoluteEvent, UnusedEvent,
};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use std::{
    collections::HashSet,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::Path,
};

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
    fn x_transformed(&self, _: i32) -> f64 {
        self.x
    }
    fn y_transformed(&self, _: i32) -> f64 {
        self.y
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
                        Ok(command) => execute(state, command, &mut held),
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
        [Some("snapshot"), None, None, None] => {
            let windows: Vec<_> = state.window_service.windows().filter(|window| state.window_service.is_client_window(window.id) && window.mapped).map(|window| {
                let card = state.motion.target(window.id).and_then(|target| {
                    let surface = state.window_service.window(target.surface_window)?;
                    Some(serde_json::json!({"x": surface.geometry.x + target.rect.x, "y": surface.geometry.y + target.rect.y, "width": target.rect.width, "height": target.rect.height}))
                });
                serde_json::json!({"id": window.id.0, "title": window.title, "active": window.activated, "parked": window.shelved, "card": card})
            }).collect();
            return Ok(
                serde_json::json!({"windows": windows, "home": state.window_service.home_revealed(), "selected": state.switcher.as_ref().map(|switcher| switcher.selected.0)}),
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
            state.process_input_event::<TestBackend>(
                InputEvent::PointerMotionAbsolute { event },
                Some("winit"),
            );
        }
        _ => anyhow::bail!("expected snapshot | key/button EVDEV down/up | motion X Y"),
    }
    Ok(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_use_evdev_keycodes_and_absolute_output_coordinates() {
        let event = TestEvent {
            code: 56,
            pressed: true,
            x: 123.0,
            y: 456.0,
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
        assert!((event.x_transformed(1920) - 123.0).abs() < f64::EPSILON);
        assert!((event.y_transformed(1080) - 456.0).abs() < f64::EPSILON);
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
