//! `ext-session-lock-v1`, from the client side: what actually locks the session.
//!
//! The compositor has implemented this protocol in full since M2 (`compositor/lege-comp/src/
//! session_lock.rs`) and ADR-0010 makes the lock screen compositor-owned — "one provider by
//! construction". What never existed was anything that *drove* it, so `Lifecycle1.Lock` returned
//! `NotSupported`: no client called `lock()`, so there was no lock surface and no lock.
//!
//! This is that client, and it lives in the shell rather than in sessiond for one reason: only
//! the shell holds the compositor's trusted connection. sessiond speaks no Wayland at all and is
//! not going to start; it emits `Lifecycle1.LockRequested` and this listens for it.
//!
//! ## What the protocol guarantees, and what it does not
//!
//! Once `locked` arrives the compositor gives keyboard focus and every pointer hit to the lock
//! surfaces and to nothing else, so the password field receives keys without asking for focus and
//! no window behind the lock screen can be reached. Crucially, **only `unlock_and_destroy` clears
//! the lock**: if this process dies while locked, the session stays locked, which is the property
//! the whole protocol exists for. That is also why the surfaces are painted before `lock()` is
//! ever called — a lock with nothing drawn on it is a black screen a person cannot tell from a
//! crash.
//!
//! Keys are decoded here with xkbcommon rather than by winit, because a lock surface is not a
//! winit window: it is a `wl_surface` this module creates and hands to the protocol.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;

/// Tell sessiond the lock is gone.
///
/// `unlock_and_destroy` above is the only thing that clears the compositor's lock, and this
/// process is the only one that calls it — so this process is the only one that can report it.
/// sessiond needs to know because session time is counted from these edges: a screen locked
/// overnight aged nothing, and without this the interval never closes.
///
/// Best-effort and deliberately quiet on failure: the session is already unlocked by the time
/// this runs, and a bookkeeping call that could not be made must not become an error in the
/// user's face.
pub async fn notify_unlocked() {
    let Ok(connection) = crate::data::session().await else {
        return;
    };
    match lege_client::proxies::LifecycleProxy::new(&connection).await {
        Ok(proxy) => {
            if let Err(error) = proxy.notify_unlocked().await {
                tracing::debug!(%error, "sessiond would not record the unlock");
            }
        }
        Err(error) => tracing::debug!(%error, "no Lifecycle1 to report the unlock to"),
    }
}

use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{
        wl_compositor::WlCompositor,
        wl_keyboard,
        wl_output::{self, WlOutput},
        wl_registry::WlRegistry,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

use crate::wayland_bridge::BridgeState;

/// One key the lock surface received, already decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LockKey {
    Text(char),
    Enter,
    Backspace,
    Escape,
}

/// Decode one xkb keysym into what a password field can use. Deliberately tiny: a lock screen
/// takes text, Enter, Backspace and Escape, and treats everything else as nothing at all.
///
/// Pure, so the one thing that would otherwise need a real keyboard on a real locked session —
/// "does pressing Enter submit" — is a test.
pub fn key_from_keysym(keysym: u32, utf8: Option<&str>) -> Option<LockKey> {
    // xkb keysym constants, spelled out rather than pulled from a header: there are four.
    const XKB_RETURN: u32 = 0xff0d;
    const XKB_KP_ENTER: u32 = 0xff8d;
    const XKB_BACKSPACE: u32 = 0xff08;
    const XKB_ESCAPE: u32 = 0xff1b;
    match keysym {
        XKB_RETURN | XKB_KP_ENTER => Some(LockKey::Enter),
        XKB_BACKSPACE => Some(LockKey::Backspace),
        XKB_ESCAPE => Some(LockKey::Escape),
        _ => {
            // A keysym with a printable string is text; a modifier or a function key has none.
            // The control-character filter matters because xkb hands back "\r" for Return and
            // "\u{8}" for Backspace as well as the keysyms above, and those must not arrive twice.
            let character = utf8?.chars().next()?;
            (!character.is_control()).then_some(LockKey::Text(character))
        }
    }
}

/// One output's lock surface: the `wl_surface`, the protocol object over it, and the buffer it
/// is painted into.
struct LockSurface {
    proxy: ExtSessionLockSurfaceV1,
    surface: WlSurface,
    buffer: Option<crate::previews::ShmBuffer>,
    /// The size the compositor configured, in surface-local pixels. `None` until it does.
    size: Option<(u32, u32)>,
}

/// What the lock needs from the registry, and the lock itself while one is up.
#[derive(Default)]
pub struct SessionLockState {
    manager: Option<ExtSessionLockManagerV1>,
    compositor: Option<WlCompositor>,
    outputs: HashMap<u32, WlOutput>,
    lock: Option<ExtSessionLockV1>,
    surfaces: Vec<LockSurface>,
    /// The compositor confirmed the lock: the screen is covered and input is ours.
    locked: bool,
    /// The compositor refused, or the lock ended without us. Either way there is nothing to
    /// unlock and the shell must stop pretending there is.
    finished: bool,
    /// Keys the lock surfaces received, for the host to drain. Cleared on every read.
    keys: Vec<LockKey>,
    /// The keymap the compositor sent, and the state derived from it.
    xkb: Option<XkbState>,
    /// Set when a surface's size changed, so the host knows to repaint.
    pub dirty: bool,
}

impl std::fmt::Debug for SessionLockState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLockState")
            .field("locked", &self.locked)
            .field("surfaces", &self.surfaces.len())
            .finish_non_exhaustive()
    }
}

/// The compositor's keymap, and the state that turns a keycode into a keysym.
struct XkbState {
    state: xkbcommon::xkb::State,
}

impl SessionLockState {
    /// Bind whichever of this module's globals `interface` is.
    pub fn bind_global(
        &mut self,
        registry: &WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<BridgeState>,
    ) {
        match interface {
            "ext_session_lock_manager_v1" => {
                self.manager = Some(registry.bind::<ExtSessionLockManagerV1, _, _>(
                    name,
                    version.min(1),
                    qh,
                    (),
                ));
            }
            "wl_compositor" => {
                // A second binding beside winit's own, so this module can make a surface without
                // asking winit for one it would then try to manage.
                self.compositor =
                    Some(registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ()));
            }
            "wl_output" => {
                self.outputs.insert(
                    name,
                    registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ()),
                );
            }
            _ => {}
        }
    }

    /// Whether the compositor offers everything a lock needs on this connection.
    pub fn available(&self) -> bool {
        self.manager.is_some() && self.compositor.is_some() && !self.outputs.is_empty()
    }

    /// Whether a lock is up. True from `lock()` returning, not from `locked` arriving: the
    /// surfaces exist and the compositor is already refusing input to everything else.
    pub fn engaged(&self) -> bool {
        self.lock.is_some()
    }

    /// Whether the compositor has confirmed the lock.
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Whether the compositor refused the lock or ended it.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The keys the lock surfaces received since the last call.
    pub fn take_keys(&mut self) -> Vec<LockKey> {
        std::mem::take(&mut self.keys)
    }

    /// Lock the session: one surface per output, each created and committed *before* the first
    /// frame, then painted by [`Self::paint`].
    ///
    /// Locking twice is not an error and not a second lock; it is the same lock.
    pub fn lock(&mut self, shm_qh: &QueueHandle<BridgeState>) -> Result<()> {
        if self.engaged() {
            return Ok(());
        }
        let manager = self
            .manager
            .as_ref()
            .ok_or_else(|| anyhow!("the compositor offers no ext_session_lock_manager_v1"))?;
        let compositor = self
            .compositor
            .as_ref()
            .ok_or_else(|| anyhow!("the compositor offers no wl_compositor on this connection"))?;
        if self.outputs.is_empty() {
            return Err(anyhow!("no outputs to lock"));
        }
        self.finished = false;
        self.locked = false;
        let lock = manager.lock(shm_qh, ());
        for output in self.outputs.values() {
            let surface = compositor.create_surface(shm_qh, ());
            let proxy = lock.get_lock_surface(&surface, output, shm_qh, ());
            self.surfaces.push(LockSurface {
                proxy,
                surface,
                buffer: None,
                size: None,
            });
        }
        self.lock = Some(lock);
        Ok(())
    }

    /// Paint every configured lock surface with `render`, which is handed the surface's size and
    /// fills a buffer of `width * height` pixels in pixelkit's `0x00RRGGBB`.
    ///
    /// A surface the compositor has not configured yet is skipped: its size is not known, and
    /// guessing one produces a lock screen that jumps.
    pub fn paint(
        &mut self,
        shm: &wayland_client::protocol::wl_shm::WlShm,
        qh: &QueueHandle<BridgeState>,
        mut render: impl FnMut(u32, u32) -> Vec<u32>,
    ) -> Result<()> {
        for surface in &mut self.surfaces {
            let Some((width, height)) = surface.size else {
                continue;
            };
            if width == 0 || height == 0 {
                continue;
            }
            let reuse = surface
                .buffer
                .as_ref()
                .is_some_and(|buffer| buffer.width == width && buffer.height == height);
            if !reuse {
                if let Some(old) = surface.buffer.take() {
                    old.destroy();
                }
                surface.buffer = Some(
                    crate::previews::allocate(
                        shm,
                        width,
                        height,
                        wayland_client::protocol::wl_shm::Format::Argb8888,
                        qh,
                    )
                    .context("allocating a lock-screen buffer")?,
                );
            }
            let buffer = surface.buffer.as_ref().expect("allocated above");
            buffer.write(&render(width, height))?;
            surface.surface.attach(Some(&buffer.buffer), 0, 0);
            surface
                .surface
                .damage_buffer(0, 0, width as i32, height as i32);
            surface.surface.commit();
        }
        self.dirty = false;
        Ok(())
    }

    /// Unlock and tear everything down. `unlock_and_destroy` is the *only* thing that clears the
    /// compositor's lock, which is why it is called before the surfaces are dropped rather than
    /// after: destroying a lock surface without unlocking leaves a locked session with nothing
    /// drawn on it.
    pub fn unlock(&mut self) {
        if let Some(lock) = self.lock.take() {
            if self.locked && !self.finished {
                lock.unlock_and_destroy();
            } else {
                // Never confirmed, or already finished: there is no lock to release, and
                // `unlock_and_destroy` on one the compositor did not grant is a protocol error.
                lock.destroy();
            }
        }
        for surface in self.surfaces.drain(..) {
            if let Some(buffer) = surface.buffer {
                buffer.destroy();
            }
            surface.proxy.destroy();
            surface.surface.destroy();
        }
        self.locked = false;
        self.finished = false;
    }

    fn surface_configured(&mut self, proxy: &ExtSessionLockSurfaceV1, size: (u32, u32)) {
        if let Some(surface) = self.surfaces.iter_mut().find(|s| &s.proxy == proxy)
            && surface.size != Some(size)
        {
            surface.size = Some(size);
            self.dirty = true;
        }
    }

    /// A key event on a lock surface. Presses only; releases say nothing a password field needs.
    fn key(&mut self, keycode: u32, pressed: bool) {
        if !pressed {
            return;
        }
        let Some(xkb) = &self.xkb else {
            return;
        };
        // Wayland keycodes are evdev's; xkb's are evdev's plus 8.
        let code = xkbcommon::xkb::Keycode::new(keycode + 8);
        let keysym = xkb.state.key_get_one_sym(code).raw();
        let utf8 = xkb.state.key_get_utf8(code);
        if let Some(key) = key_from_keysym(keysym, Some(utf8.as_str())) {
            self.keys.push(key);
        }
    }

    /// Compile the keymap the compositor sent.
    ///
    /// Read out of the fd rather than mapped: `xkbcommon`'s `new_from_fd` is `unsafe` and this
    /// crate forbids `unsafe`, and the keymap is NUL-terminated text a few tens of kilobytes
    /// long — `pread` into a `String` is the same thing `crate::previews` does with its shm
    /// pools, for the same reason.
    fn keymap(
        &mut self,
        format: WEnum<wl_keyboard::KeymapFormat>,
        fd: std::os::fd::OwnedFd,
        size: u32,
    ) {
        if !matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
            return;
        }
        let file = std::fs::File::from(fd);
        let mut bytes = vec![0u8; size as usize];
        if let Err(error) = std::os::unix::fs::FileExt::read_exact_at(&file, &mut bytes, 0) {
            tracing::warn!(%error, "could not read the compositor's keymap");
            return;
        }
        // The keymap is NUL-terminated; `from_utf8_lossy` would keep the terminator as a
        // character and `new_from_string` would refuse the result.
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text.trim_end_matches('\0'),
            Err(error) => {
                tracing::warn!(%error, "the compositor's keymap is not valid UTF-8");
                return;
            }
        };
        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        if let Some(keymap) = xkbcommon::xkb::Keymap::new_from_string(
            &context,
            text.to_owned(),
            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            self.xkb = Some(XkbState {
                state: xkbcommon::xkb::State::new(&keymap),
            });
        } else {
            tracing::warn!("could not compile the compositor's keymap");
        }
    }

    fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(xkb) = &mut self.xkb {
            xkb.state
                .update_mask(depressed, latched, locked, 0, 0, group);
        }
    }
}

// --- dispatch -------------------------------------------------------------------------------

impl Dispatch<ExtSessionLockManagerV1, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtSessionLockManagerV1,
        _event: <ExtSessionLockManagerV1 as wayland_client::Proxy>::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ExtSessionLockV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => {
                state.session_lock.locked = true;
                state.session_lock.dirty = true;
            }
            ext_session_lock_v1::Event::Finished => {
                // The compositor refused the lock, or another client already holds one. Neither
                // is recoverable here, and pretending otherwise would show a lock screen over an
                // unlocked session — worse than not locking at all, because it looks locked.
                state.session_lock.finished = true;
                tracing::warn!("the compositor finished the session lock without granting it");
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            // Acknowledged before anything is drawn: the protocol requires the ack to precede
            // the commit that carries the buffer for that configure.
            proxy.ack_configure(serial);
            state
                .session_lock
                .surface_configured(proxy, (width, height));
        }
    }
}

impl Dispatch<WlCompositor, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: wayland_client::protocol::wl_surface::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlOutput,
        _event: wl_output::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Geometry and mode are the compositor's business; the lock surface is told its own size
        // by `configure`, which is the only size that is ever correct for it.
    }
}

/// The keyboard events the lock cares about, routed from the bridge's own `wl_keyboard`.
///
/// The bridge binds one keyboard already, to see input serials for the clipboard. While locked
/// the compositor focuses the lock surface, so that same keyboard is where the password arrives.
pub(crate) fn keyboard_event(state: &mut BridgeState, event: &wl_keyboard::Event) {
    match event {
        wl_keyboard::Event::Keymap { format, fd, size } => {
            // The fd must be duplicated: the bridge's clipboard half does not read the keymap,
            // but the event carries one fd and this consumes it.
            match fd.try_clone() {
                Ok(fd) => state.session_lock.keymap(*format, fd, *size),
                Err(error) => tracing::warn!(%error, "could not take the compositor's keymap"),
            }
        }
        wl_keyboard::Event::Key {
            key,
            state: key_state,
            ..
        } => {
            if state.session_lock.engaged() {
                let pressed = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                state.session_lock.key(*key, pressed);
            }
        }
        wl_keyboard::Event::Modifiers {
            mods_depressed,
            mods_latched,
            mods_locked,
            group,
            ..
        } => state
            .session_lock
            .modifiers(*mods_depressed, *mods_latched, *mods_locked, *group),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four keys a password field needs, and nothing else. This is the one part of the lock
    /// client that can be checked without a compositor, and it is also the part that would fail
    /// silently: a Return that arrives as both `Enter` and a control character types a password
    /// nobody can enter.
    #[test]
    fn a_lock_surface_decodes_text_enter_backspace_and_escape_and_ignores_the_rest() {
        assert_eq!(key_from_keysym(0xff0d, Some("\r")), Some(LockKey::Enter));
        assert_eq!(key_from_keysym(0xff8d, Some("\r")), Some(LockKey::Enter));
        assert_eq!(
            key_from_keysym(0xff08, Some("\u{8}")),
            Some(LockKey::Backspace)
        );
        assert_eq!(
            key_from_keysym(0xff1b, Some("\u{1b}")),
            Some(LockKey::Escape)
        );

        assert_eq!(key_from_keysym(0x0061, Some("a")), Some(LockKey::Text('a')));
        assert_eq!(key_from_keysym(0x0041, Some("A")), Some(LockKey::Text('A')));
        assert_eq!(key_from_keysym(0x0020, Some(" ")), Some(LockKey::Text(' ')));
        assert_eq!(key_from_keysym(0x00e9, Some("é")), Some(LockKey::Text('é')));

        // Modifiers and function keys produce no text and must produce no key: Shift arriving as
        // a character is a password with an invisible extra letter in it.
        assert_eq!(key_from_keysym(0xffe1, Some("")), None, "Shift_L");
        assert_eq!(key_from_keysym(0xffe3, None), None, "Control_L");
        assert_eq!(key_from_keysym(0xffbe, Some("")), None, "F1");
        // A control character that is not one of the three named keys is still not text.
        assert_eq!(key_from_keysym(0x0000, Some("\u{1}")), None);
    }

    #[test]
    fn a_lock_with_no_globals_refuses_rather_than_pretending() {
        let mut lock = SessionLockState::default();
        assert!(!lock.available());
        assert!(!lock.engaged());
        assert!(!lock.locked());
        // Unlocking one that was never taken is a no-op, not a protocol error.
        lock.unlock();
        assert!(!lock.engaged());
    }
}
