//! Fullscreen playback inhibition. Fullscreen alone and generic client inhibitors are insufficient.
//!
//! The D-Bus path provides a playing-video reason and an authenticated process ID. Wayland
//! provides an exact surface, but no reason, so it also needs the Video content-type hint.
//! Both sources are re-evaluated when visibility, fullscreen, mapping or content changes.

use crate::state::{Backend, TemplarCompState};
use smithay::{
    reexports::{
        calloop::{
            LoopHandle,
            channel::{self, Event},
        },
        wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type,
        wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::{get_parent, with_states},
        content_type::ContentTypeSurfaceCachedState,
    },
};
use std::collections::{BTreeSet, HashMap};
use templar_window::{Presentation, Rect, WindowRole};

#[derive(Debug, Default)]
pub struct VideoIdleState {
    wayland: HashMap<ObjectId, (WlSurface, usize)>,
    playing_video_pids: BTreeSet<u32>,
}

impl VideoIdleState {
    pub fn inhibit(&mut self, surface: WlSurface) {
        let entry = self.wayland.entry(surface.id()).or_insert((surface, 0));
        entry.1 = entry.1.saturating_add(1);
    }

    pub fn uninhibit(&mut self, surface: &WlSurface) {
        if let Some((_, count)) = self.wayland.get_mut(&surface.id()) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.wayland.remove(&surface.id());
            }
        }
    }

    pub fn forget_surface(&mut self, surface: &WlSurface) {
        self.wayland.remove(&surface.id());
    }
}

/// The compositor's thread never performs a bus call. This channel wakes it only when the
/// set of processes actively requesting a video wake lock changes.
pub fn install<B: Backend + 'static>(
    loop_handle: &LoopHandle<'static, TemplarCompState<B>>,
) -> impl Fn(Vec<u32>) + Send + Sync + 'static {
    let (sender, receiver) = channel::channel::<Vec<u32>>();
    loop_handle
        .insert_source(receiver, |event, (), state: &mut TemplarCompState<B>| {
            if let Event::Msg(pids) = event {
                state.video_idle.playing_video_pids = pids.into_iter().collect();
                refresh(state);
            }
        })
        .expect("inserting the playing-video channel source");
    move |pids| {
        let _ = sender.send(pids);
    }
}

fn video_hint(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        *states
            .cached_state
            .get::<ContentTypeSurfaceCachedState>()
            .current()
            .content_type()
            == Type::Video
    })
}

fn fullscreen_video(fullscreen: bool, visible: bool, playing_video: bool) -> bool {
    fullscreen && visible && playing_video
}

fn covers(higher: Rect, lower: Rect) -> bool {
    higher.x <= lower.x
        && higher.y <= lower.y
        && higher.right() >= lower.right()
        && higher.bottom() >= lower.bottom()
}

pub fn refresh<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if state.video_idle.wayland.is_empty()
        && state.video_idle.playing_video_pids.is_empty()
        && !state.idle.inhibited()
    {
        return;
    }
    state
        .video_idle
        .wayland
        .retain(|_, (surface, _)| surface.is_alive());
    let normal = matches!(state.window_service.presentation(), Presentation::Normal)
        && !state.session_lock.locked;
    // WindowService owns stacking/geometry. A completely covered fullscreen player is not
    // watching activity, even though its protocol toplevel remains mapped beneath another app.
    let mut exposed: Vec<&templar_window::WindowRecord> = Vec::new();
    if normal {
        for id in state.window_service.stacking_order() {
            let Some(record) = state.window_service.window(id) else {
                continue;
            };
            if record.role == WindowRole::Application
                && state.window_service.is_visible(id)
                && !exposed
                    .iter()
                    .any(|higher| covers(higher.geometry, record.geometry))
            {
                exposed.push(record);
            }
        }
    }
    let count = exposed
        .iter()
        .filter(|record| {
            if !record.fullscreen {
                return false;
            }
            // ScreenSaver carries a PID rather than a window handle. Reject ambiguous
            // multi-window playback instead of attributing one window's video to another.
            let dbus_video = record.peer_pid != 0
                && state
                    .video_idle
                    .playing_video_pids
                    .contains(&record.peer_pid)
                && exposed
                    .iter()
                    .filter(|other| other.peer_pid == record.peer_pid)
                    .count()
                    == 1;
            let wayland_video = state.video_idle.wayland.values().any(|(surface, _)| {
                let mut root = surface.clone();
                while let Some(parent) = get_parent(&root) {
                    root = parent;
                }
                state.windows.window_id_for(&root.id()) == Some(record.id)
                    && (video_hint(surface) || video_hint(&root))
            });
            fullscreen_video(record.fullscreen, true, dbus_video || wayland_video)
        })
        .count();
    if state.idle.set_inhibitor_count(count) {
        state.arm_idle_timer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, protocol::wl_registry};
    use wayland_protocols::wp::{
        content_type::v1::client::{wp_content_type_manager_v1, wp_content_type_v1},
        idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1},
    };

    #[derive(Default)]
    struct VideoClient {
        compositor: Option<wayland_client::protocol::wl_compositor::WlCompositor>,
        content: Option<wp_content_type_manager_v1::WpContentTypeManagerV1>,
        idle: Option<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for VideoClient {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                    "wp_content_type_manager_v1" => {
                        state.content = Some(registry.bind(name, 1, qh, ()));
                    }
                    "zwp_idle_inhibit_manager_v1" => {
                        state.idle = Some(registry.bind(name, 1, qh, ()));
                    }
                    _ => {}
                }
            }
        }
    }

    wayland_client::delegate_noop!(VideoClient: ignore wayland_client::protocol::wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(VideoClient: ignore wayland_client::protocol::wl_surface::WlSurface);
    wayland_client::delegate_noop!(VideoClient: ignore wp_content_type_manager_v1::WpContentTypeManagerV1);
    wayland_client::delegate_noop!(VideoClient: ignore wp_content_type_v1::WpContentTypeV1);
    wayland_client::delegate_noop!(VideoClient: ignore zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
    wayland_client::delegate_noop!(VideoClient: ignore zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);

    enum VideoCommand {
        VideoPending,
        Commit,
        Pause,
        Resume,
        DestroyHint,
        DestroySurface,
        Stop,
    }

    fn dispatch_until(
        event_loop: &mut smithay::reexports::calloop::EventLoop<
            TemplarCompState<crate::backend::check::CheckBackendData>,
        >,
        state: &mut TemplarCompState<crate::backend::check::CheckBackendData>,
        replies: &std::sync::mpsc::Receiver<u32>,
    ) -> u32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(reply) = replies.try_recv() {
                return reply;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "video client roundtrip timed out"
            );
            event_loop
                .dispatch(Some(std::time::Duration::from_millis(10)), state)
                .unwrap();
            state.display_handle.flush_clients().unwrap();
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn idle_wayland_video_hint_is_committed_and_playback_inhibitor_releases() {
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut state, _) =
            crate::backend::check::build_state(&event_loop, runtime.handle()).unwrap();
        state
            .window_service
            .on_outputs_changed(vec![templar_window::Output::new(
                "fixture",
                Rect::new(0, 0, 1920, 1080),
                1000,
            )]);
        let (server, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut dh = state.display_handle.clone();
        let owner = dh
            .insert_client(
                server,
                std::sync::Arc::new(crate::state::ClientState::default()),
            )
            .unwrap();
        let (commands, command_rx) = std::sync::mpsc::channel();
        let (replies, reply_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let connection = Connection::from_socket(peer).unwrap();
            let mut queue = connection.new_event_queue();
            let qh = queue.handle();
            let mut globals = VideoClient::default();
            connection.display().get_registry(&qh, ());
            queue.roundtrip(&mut globals).unwrap();
            let surface = globals.compositor.as_ref().unwrap().create_surface(&qh, ());
            let hint =
                globals
                    .content
                    .as_ref()
                    .unwrap()
                    .get_surface_content_type(&surface, &qh, ());
            hint.set_content_type(wp_content_type_v1::Type::Game);
            let mut inhibitor = Some(globals.idle.as_ref().unwrap().create_inhibitor(
                &surface,
                &qh,
                (),
            ));
            surface.commit();
            queue.roundtrip(&mut globals).unwrap();
            let id = surface.id().protocol_id();
            replies.send(id).unwrap();
            while let Ok(command) = command_rx.recv() {
                match command {
                    VideoCommand::VideoPending => {
                        hint.set_content_type(wp_content_type_v1::Type::Video);
                    }
                    VideoCommand::Commit => surface.commit(),
                    VideoCommand::Pause => inhibitor.take().unwrap().destroy(),
                    VideoCommand::Resume => {
                        inhibitor = Some(globals.idle.as_ref().unwrap().create_inhibitor(
                            &surface,
                            &qh,
                            (),
                        ));
                    }
                    VideoCommand::DestroyHint => hint.destroy(),
                    VideoCommand::DestroySurface => surface.destroy(),
                    VideoCommand::Stop => break,
                }
                queue.roundtrip(&mut globals).unwrap();
                replies.send(id).unwrap();
            }
        });
        let protocol_id = dispatch_until(&mut event_loop, &mut state, &reply_rx);
        let surface = owner
            .object_from_protocol_id::<WlSurface>(&dh, protocol_id)
            .unwrap();
        let window = state.window_service.on_toplevel_mapped(
            4242,
            templar_window::ClientKind::Application,
            templar_window::WindowClaims::default(),
            "player".into(),
            Rect::new(0, 0, 400, 300),
        );
        state.windows.mark_mapped(surface.clone(), window);
        state.window_service.set_fullscreen(window, true);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "a fullscreen game with a generic inhibitor is insufficient"
        );
        let step = |command, event_loop: &mut _, state: &mut _| {
            commands.send(command).unwrap();
            dispatch_until(event_loop, state, &reply_rx);
        };
        step(VideoCommand::VideoPending, &mut event_loop, &mut state);
        assert!(!video_hint(&surface), "content hints are double buffered");
        assert!(!state.idle.inhibited());
        step(VideoCommand::Commit, &mut event_loop, &mut state);
        assert!(video_hint(&surface));
        assert!(
            state.idle.inhibited(),
            "a committed fullscreen Video plus active inhibitor counts"
        );
        step(VideoCommand::Pause, &mut event_loop, &mut state);
        assert!(
            !state.idle.inhibited(),
            "pause destroys the inhibitor even while Video hint remains"
        );
        step(VideoCommand::Resume, &mut event_loop, &mut state);
        assert!(state.idle.inhibited());
        step(VideoCommand::DestroyHint, &mut event_loop, &mut state);
        assert!(
            state.idle.inhibited(),
            "hint destruction is double buffered too"
        );
        step(VideoCommand::Commit, &mut event_loop, &mut state);
        assert!(
            !state.idle.inhibited(),
            "removed Video hint releases eligibility"
        );
        step(VideoCommand::DestroySurface, &mut event_loop, &mut state);
        assert!(state.video_idle.wayland.is_empty());
        assert!(!state.idle.inhibited());
        commands.send(VideoCommand::Stop).unwrap();
        client.join().unwrap();
    }

    #[test]
    fn idle_fullscreen_apps_audio_and_paused_video_do_not_inhibit() {
        assert!(!fullscreen_video(true, true, false));
        assert!(!fullscreen_video(false, true, true));
        assert!(!fullscreen_video(true, false, true));
        assert!(fullscreen_video(true, true, true));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn idle_video_tracks_exact_process_fullscreen_and_visibility_transitions() {
        let event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut state, _) =
            crate::backend::check::build_state(&event_loop, runtime.handle()).unwrap();
        state
            .window_service
            .on_outputs_changed(vec![templar_window::Output::new(
                "fixture",
                templar_window::Rect::new(0, 0, 1920, 1080),
                1000,
            )]);
        let _ = state
            .idle
            .set_timeout(Some(std::time::Duration::from_secs(60)));
        let map = |state: &mut TemplarCompState<crate::backend::check::CheckBackendData>, pid| {
            state.window_service.on_toplevel_mapped(
                pid,
                templar_window::ClientKind::Application,
                templar_window::WindowClaims::default(),
                "fixture".into(),
                templar_window::Rect::new(0, 0, 400, 300),
            )
        };
        let browser = map(&mut state, 4242);
        let unrelated = map(&mut state, 4343);
        state.window_service.set_fullscreen(unrelated, true);
        state.video_idle.playing_video_pids.insert(4242);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "a windowed video cannot inhibit another app's fullscreen"
        );
        state.window_service.set_fullscreen(browser, true);
        state.window_service.activate(browser);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        assert!(state.idle.next_delay().is_none());
        state.session_lock.locked = true;
        refresh(&mut state);
        assert!(!state.idle.inhibited(), "locking hides playback");
        smithay::wayland::session_lock::SessionLockHandler::unlock(&mut state);
        assert!(
            state.idle.inhibited(),
            "unlock re-evaluates the existing playback source"
        );
        state.window_service.activate(unrelated);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "a fullscreen app covering the player releases inhibition"
        );
        state.window_service.activate(browser);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        state.window_service.on_outputs_changed(vec![
            templar_window::Output::new("fixture", Rect::new(0, 0, 1920, 1080), 1000),
            templar_window::Output::new("second", Rect::new(1920, 0, 1920, 1080), 1000),
        ]);
        let sibling = map(&mut state, 4242);
        state
            .window_service
            .set_geometry(sibling, Rect::new(2020, 30, 400, 300));
        state.window_service.activate(browser);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "PID-only playback is ambiguous with another exposed browser window"
        );
        state.window_service.on_toplevel_destroyed(sibling);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        state.video_idle.playing_video_pids.clear();
        refresh(&mut state);
        assert!(!state.idle.inhibited(), "pause retires the source");
        assert!(state.idle.next_delay().unwrap() > std::time::Duration::from_secs(59));
        state.video_idle.playing_video_pids.insert(4242);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        state.window_service.shelve(browser);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "a shelved player cannot hold the screen awake"
        );
        state.window_service.unshelve(browser);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        state.window_service.set_home_revealed(true);
        refresh(&mut state);
        assert!(!state.idle.inhibited(), "Home hides playback");
        state.window_service.set_home_revealed(false);
        refresh(&mut state);
        assert!(state.idle.inhibited());
        state.window_service.set_mapped(browser, false);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "unmapped video is not visually relevant"
        );
        state.window_service.set_mapped(browser, true);
        state.window_service.set_fullscreen(browser, false);
        refresh(&mut state);
        assert!(
            !state.idle.inhibited(),
            "leaving fullscreen releases inhibition"
        );
    }
}
