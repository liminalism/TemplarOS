//! Playback wake locks from X11 browsers and players, without guessing from fullscreen alone.
//!
//! Firefox uses `video-playing` and Chromium's VideoWakeLock uses `Video Wake Lock`.
//! This compatibility API only forwards explicitly video-related requests; the compositor
//! separately requires a visible fullscreen window belonging to the bus sender's actual PID.
//! Clients release their cookie on pause/stop. Generic screen/presentation/audio wake locks do
//! not become video activity. This deliberately does not implement portal Inhibit or MPRIS:
//! neither generic idle flags nor MPRIS Playing establish that the media has video frames.

use futures_util::StreamExt;
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};
use templar_types::dbus::screensaver;
use zbus::{Connection, fdo, message::Header};

const NAME: &str = screensaver::BUS_NAME;
const MAX_COOKIES: usize = 1024;
const MAX_PER_SENDER: usize = 64;
type Publish = Arc<dyn Fn(Vec<u32>) + Send + Sync>;

#[derive(Debug)]
struct Inhibitor {
    owner: String,
    video_pid: Option<u32>,
}

#[derive(Default)]
struct Cookies {
    next: u32,
    entries: BTreeMap<u32, Inhibitor>,
}

impl Cookies {
    fn insert(&mut self, owner: &str, pid: u32, reason: &str) -> fdo::Result<u32> {
        if self.entries.len() >= MAX_COOKIES
            || self
                .entries
                .values()
                .filter(|entry| entry.owner == owner)
                .count()
                >= MAX_PER_SENDER
        {
            return Err(fdo::Error::LimitsExceeded("too many wake locks".into()));
        }
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            if !self.entries.contains_key(&self.next) {
                break;
            }
        }
        self.entries.insert(
            self.next,
            Inhibitor {
                owner: owner.to_owned(),
                video_pid: (pid != 0 && video_reason(reason)).then_some(pid),
            },
        );
        Ok(self.next)
    }

    fn remove(&mut self, owner: &str, cookie: u32) -> fdo::Result<()> {
        match self.entries.get(&cookie) {
            Some(entry) if entry.owner != owner => {
                return Err(fdo::Error::AccessDenied(
                    "wake lock belongs to another sender".into(),
                ));
            }
            _ => {}
        }
        self.entries.remove(&cookie);
        Ok(())
    }

    fn disconnect(&mut self, owner: &str) {
        self.entries.retain(|_, entry| entry.owner != owner);
    }

    fn video_pids(&self) -> Vec<u32> {
        let mut pids: Vec<_> = self
            .entries
            .values()
            .filter_map(|entry| entry.video_pid)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

fn video_reason(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "video-playing"
            | "video wake lock"
            | "video playback"
            | "playing video"
            | "playing a video"
            | "video playing"
    )
}

#[derive(Clone)]
struct ScreenSaver {
    cookies: Arc<Mutex<Cookies>>,
    publish: Publish,
}

impl ScreenSaver {
    fn change(&self, change: impl FnOnce(&mut Cookies) -> fdo::Result<()>) -> fdo::Result<()> {
        let mut cookies = self.cookies.lock();
        let before = cookies.video_pids();
        change(&mut cookies)?;
        let after = cookies.video_pids();
        if after != before {
            (self.publish)(after);
        }
        Ok(())
    }
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    async fn inhibit(
        &self,
        application_name: &str,
        reason: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<u32> {
        // Compatibility clients provide this label; identity comes from bus credentials.
        let _ = application_name;
        let owner = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing sender".into()))?;
        let bus = fdo::DBusProxy::new(connection).await?;
        let pid = bus
            .get_connection_unix_process_id(owner.clone().into())
            .await?;
        let mut cookie = 0;
        self.change(|cookies| {
            cookie = cookies.insert(owner.as_str(), pid, reason)?;
            Ok(())
        })?;
        // Resolve/insert can race a disconnect already consumed by the owner watcher. Check
        // afterwards so a dead sender can never leave a freshly inserted cookie behind.
        if !bus
            .name_has_owner(owner.clone().into())
            .await
            .unwrap_or(false)
        {
            self.change(|cookies| {
                cookies.disconnect(owner.as_str());
                Ok(())
            })?;
        }
        Ok(cookie)
    }

    #[zbus(name = "UnInhibit")]
    // zbus 5's interface macro passes message.header() by value; a borrowed Header parameter
    // is not supported by its generated dispatch call.
    #[allow(clippy::needless_pass_by_value)]
    fn uninhibit(&self, cookie: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let owner = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing sender".into()))?;
        self.change(|cookies| cookies.remove(owner.as_str(), cookie))
    }
}

/// Register only in the installed compositor's session-activation path. Nested backends and
/// headless checks never acquire a desktop compatibility name on the person's live bus.
pub fn spawn(handle: &tokio::runtime::Handle, publish: impl Fn(Vec<u32>) + Send + Sync + 'static) {
    let publish: Publish = Arc::new(publish);
    handle.spawn(async move {
        let result = async {
            let connection = Connection::session().await?;
            serve(connection, Arc::clone(&publish)).await
        }
        .await;
        // A vanished bus or another desktop owning ScreenSaver must release this source.
        publish(Vec::new());
        if let Err(error) = result {
            tracing::warn!(%error, "video wake-lock compatibility unavailable");
        }
    });
}

async fn serve(connection: Connection, publish: Publish) -> zbus::Result<()> {
    let bus = fdo::DBusProxy::new(&connection).await?;
    let mut owners = bus.receive_name_owner_changed().await?;
    let api = ScreenSaver {
        cookies: Arc::new(Mutex::new(Cookies::default())),
        publish,
    };
    for path in [screensaver::FIREFOX_PATH, screensaver::PATH] {
        connection.object_server().at(path, api.clone()).await?;
    }
    // Do not replace or queue behind an existing desktop owner.
    connection
        .request_name_with_flags(NAME, fdo::RequestNameFlags::DoNotQueue.into())
        .await?;
    tracing::info!("fullscreen video wake-lock compatibility ready");
    while let Some(event) = owners.next().await {
        let args = event.args()?;
        if args.new_owner().is_none() && args.name().as_str().starts_with(':') {
            api.change(|cookies| {
                cookies.disconnect(args.name().as_str());
                Ok(())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_idle_accepts_playback_reasons_and_rejects_generic_app_locks() {
        for reason in [
            "video-playing",
            "Video Wake Lock",
            "Video playback",
            "Playing video",
        ] {
            assert!(video_reason(reason), "{reason}");
        }
        for reason in [
            "screen",
            "audio-playing",
            "Playing music",
            "presentation",
            "game",
            "Web",
        ] {
            assert!(!video_reason(reason), "{reason}");
        }
    }

    #[test]
    fn video_idle_pause_disconnect_and_owner_ownership_retire_cookies() {
        let mut cookies = Cookies::default();
        let video = cookies.insert(":1.10", 100, "video-playing").unwrap();
        let nested = cookies.insert(":1.10", 100, "Video playback").unwrap();
        let audio = cookies.insert(":1.20", 200, "audio-playing").unwrap();
        assert_eq!(cookies.video_pids(), [100]);
        assert!(cookies.remove(":1.20", video).is_err());
        cookies.remove(":1.10", video).unwrap();
        assert_eq!(cookies.video_pids(), [100]);
        cookies.remove(":1.10", nested).unwrap();
        assert!(cookies.video_pids().is_empty());
        cookies.insert(":1.10", 100, "video-playing").unwrap();
        cookies.disconnect(":1.10");
        assert!(cookies.video_pids().is_empty());
        assert!(cookies.entries.contains_key(&audio));
    }

    #[test]
    fn video_idle_cookie_limits_and_wrap_do_not_replace_live_requests() {
        let mut cookies = Cookies::default();
        let first = cookies.insert(":1.1", 1, "video-playing").unwrap();
        cookies.next = u32::MAX;
        assert_ne!(cookies.insert(":1.1", 1, "video-playing").unwrap(), first);
        for _ in 2..MAX_PER_SENDER {
            cookies.insert(":1.1", 1, "video-playing").unwrap();
        }
        assert!(cookies.insert(":1.1", 1, "video-playing").is_err());
        assert!(cookies.entries.contains_key(&first));
    }

    async fn private_bus() -> (tokio::process::Child, String) {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1", "--nopidfile"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let address = BufReader::new(child.stdout.take().unwrap())
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        (child, address)
    }

    async fn connect(address: &str) -> Connection {
        zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn video_idle_private_bus_uses_real_sender_credentials_and_both_client_paths() {
        let (mut daemon, address) = private_bus().await;
        let server_connection = connect(&address).await;
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve(
            server_connection,
            Arc::new(move |pids| {
                let _ = updates.send(pids);
            }),
        ));
        let firefox = connect(&address).await;
        let chrome = connect(&address).await;
        let bus = fdo::DBusProxy::new(&firefox).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !bus.name_has_owner(NAME.try_into().unwrap()).await.unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let ff = zbus::Proxy::new(&firefox, NAME, "/ScreenSaver", NAME)
            .await
            .unwrap();
        let chromium = zbus::Proxy::new(&chrome, NAME, "/org/freedesktop/ScreenSaver", NAME)
            .await
            .unwrap();
        let generic: u32 = ff.call("Inhibit", &("fixture", "screen")).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "a generic wake lock is not video playback"
        );
        let video: u32 = ff
            .call("Inhibit", &("spoofed application name", "video-playing"))
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap(), [std::process::id()]);
        let foreign: zbus::Result<()> = chromium.call("UnInhibit", &video).await;
        assert!(
            foreign.is_err(),
            "another connection cannot release Firefox's cookie"
        );
        ff.call::<_, _, ()>("UnInhibit", &video).await.unwrap();
        assert!(
            rx.recv().await.unwrap().is_empty(),
            "pause releases the video source"
        );
        ff.call::<_, _, ()>("UnInhibit", &generic).await.unwrap();
        let _video: u32 = chromium
            .call("Inhibit", &("fixture", "Video Wake Lock"))
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap(), [std::process::id()]);
        chrome.close().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .unwrap()
                .unwrap()
                .is_empty(),
            "disconnect retires leaked video cookies"
        );
        server.abort();
        let _ = server.await;
        daemon.kill().await.unwrap();
        let _ = daemon.wait().await;
    }

    #[tokio::test]
    async fn video_idle_private_bus_preserves_an_existing_screensaver_owner() {
        let (mut daemon, address) = private_bus().await;
        let existing = connect(&address).await;
        existing.request_name(NAME).await.unwrap();
        let contender = connect(&address).await;
        assert!(serve(contender, Arc::new(|_| {})).await.is_err());
        let bus = fdo::DBusProxy::new(&existing).await.unwrap();
        assert_eq!(
            bus.get_name_owner(NAME.try_into().unwrap()).await.unwrap(),
            existing.unique_name().unwrap().clone()
        );
        daemon.kill().await.unwrap();
        let _ = daemon.wait().await;
    }
}
