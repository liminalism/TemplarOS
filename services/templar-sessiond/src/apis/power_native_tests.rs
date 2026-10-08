//! Actual policy owner against owned buses, pipe FDs and a sysfs-shaped directory.
//! Socket samples below are controlled policy inputs, NOT compositor/KMS lock proof.
use super::*;
use crate::apis::{backlight::{BacklightApi, LogindBrightness, SysfsBacklight}, lifecycle::spawn_sleep_watch};
use std::{io::Read as _, path::{Path, PathBuf}, sync::atomic::{AtomicBool, AtomicUsize, Ordering}};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use templar_types::compositor_control::{ControlError, ControlReply};
use zbus::fdo;

struct PrivateBus { child: tokio::process::Child, address: String }
impl PrivateBus {
    async fn new() -> Result<Self> {
        let mut child = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn()?;
        let mut output = tokio::io::BufReader::new(child.stdout.take().context("bus stdout")?);
        let mut address = String::new();
        tokio::time::timeout(CALL_DEADLINE, output.read_line(&mut address)).await??;
        ensure!(address.starts_with("unix:"), "private bus did not return its owned Unix address");
        Ok(Self { child, address: address.trim().into() })
    }
    async fn connect(&self) -> Result<zbus::Connection> {
        Ok(zbus::connection::Builder::address(self.address.as_str())?.build().await?)
    }
    async fn stop(mut self) -> Result<()> { self.child.kill().await?; self.child.wait().await?; Ok(()) }
}
struct PipeProof { what: String, closed: oneshot::Receiver<()> }
#[derive(Default)]
struct ProducerState {
    pipes: Mutex<Vec<PipeProof>>,
    sleeps: AtomicUsize,
    brightness_writes: AtomicUsize,
    battery: AtomicBool,
}
struct LoginProducer(Arc<ProducerState>);
#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl LoginProducer {
    fn can_suspend(&self) -> &str { "yes" } // Producer policy input, not physical authorization.
    fn can_hibernate(&self) -> &str { "na" }
    #[zbus(property, name = "InhibitDelayMaxUSec")]
    fn inhibit_delay_max_usec(&self) -> u64 { 250_000 }
    fn inhibit(&self, what: &str, _who: &str, _why: &str, mode: &str) -> fdo::Result<OwnedFd> {
        if what != "sleep" || mode != "delay" { return Err(fdo::Error::NotSupported("fixture has no lid/device action".into())); }
        let (mut reader, writer) = std::io::pipe().map_err(crate::apis::failed)?;
        let (closed, received) = oneshot::channel();
        std::thread::spawn(move || {
            let mut byte = [0];
            if matches!(reader.read(&mut byte), Ok(0)) { let _ = closed.send(()); }
        });
        self.0.pipes.lock().push(PipeProof { what: what.into(), closed: received });
        Ok(std::os::fd::OwnedFd::from(writer).into())
    }
    fn suspend(&self, _interactive: bool) -> fdo::Result<()> {
        self.0.sleeps.fetch_add(1, Ordering::SeqCst);
        Err(fdo::Error::NotSupported("no physical sleep endpoint in this private fixture".into()))
    }
    fn hibernate(&self, _interactive: bool) -> fdo::Result<()> {
        self.0.sleeps.fetch_add(1, Ordering::SeqCst);
        Err(fdo::Error::NotSupported("no physical hibernation endpoint in this private fixture".into()))
    }
}
struct UPowerProducer(Arc<ProducerState>);
#[zbus::interface(name = "org.freedesktop.UPower")]
impl UPowerProducer {
    #[zbus(property)] fn on_battery(&self) -> bool { self.0.battery.load(Ordering::SeqCst) }
    #[zbus(property)] fn lid_is_present(&self) -> bool { false }
    #[zbus(property)] fn lid_is_closed(&self) -> bool { false }
}
struct BrightnessProducer { state: Arc<ProducerState>, root: PathBuf }
#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl BrightnessProducer {
    fn set_brightness(&self, subsystem: &str, device: &str, value: u32) -> fdo::Result<()> {
        if subsystem != "backlight" || device != "owned-panel" || value > 100 {
            return Err(fdo::Error::InvalidArgs("not an owned fixture backlight".into()));
        }
        std::fs::write(self.root.join("owned-panel/brightness"), value.to_string()).map_err(crate::apis::failed)?;
        self.state.brightness_writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct SocketFixture {
    activity: Arc<Mutex<ActivityObservation>>,
    stop: broadcast::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    path: PathBuf,
}
impl Drop for SocketFixture { fn drop(&mut self) { let _ = self.stop.send(()); self.task.abort(); } }
impl SocketFixture {
    async fn new(root: &Path) -> Result<Self> {
        let path = root.join(format!("owned-control-{}.sock", uuid::Uuid::new_v4()));
        let listener = tokio::net::UnixListener::bind(&path)?;
        let revision = Revision::new_owner();
        let activity = Arc::new(Mutex::new(ActivityObservation {
            revision, display_revision: revision, input_revision: revision, activity_serial: 1,
            idle_elapsed_ms: 0, session_active: true, locked: false, lock_generation: None,
            protected_covered: false, idle: false, blanked: false, inhibited: false,
            idle_timeout_ms: None, outputs: Vec::new(),
            idle_control: Capability::Available { requires_restart: false },
            input_control: Capability::Available { requires_restart: false },
            display_control: Capability::Available { requires_restart: false },
        }));
        let (stop, _) = broadcast::channel(4);
        let owned = activity.clone();
        let cancellation = stop.clone();
        let task = tokio::spawn(async move {
            let mut stopped = cancellation.subscribe();
            loop {
                let accepted = tokio::select! { accepted = listener.accept() => accepted, _ = stopped.recv() => break };
                let Ok((mut socket, _)) = accepted else { break; };
                let state = owned.clone();
                let mut stopped = cancellation.subscribe();
                tokio::spawn(async move {
                    let operation = async {
                        let length = socket.read_u32().await? as usize;
                        ensure!(length > 0 && length <= templar_types::compositor_control::MAX_CONTROL_FRAME, "invalid owned frame");
                        let mut bytes = vec![0; length];
                        socket.read_exact(&mut bytes).await?;
                        let request: ControlRequest = serde_json::from_slice(&bytes)?;
                        let subscribed = request == ControlRequest::SubscribeActivity;
                        let result = {
                            let mut state = state.lock();
                            match request {
                                ControlRequest::Activity | ControlRequest::SubscribeActivity => Ok(ControlResponse::Activity(state.clone())),
                                ControlRequest::SetIdleTimeout { expected, timeout_ms } if expected == state.revision => {
                                    state.idle_timeout_ms = timeout_ms;
                                    state.revision.generation += 1;
                                    Ok(ControlResponse::Activity(state.clone()))
                                }
                                _ => Err(ControlError { code: "fixture_unavailable".into(), message: "no physical output/device endpoint".into() }),
                            }
                        };
                        let reply = serde_json::to_vec(&ControlReply { result })?;
                        socket.write_u32(u32::try_from(reply.len())?).await?;
                        socket.write_all(&reply).await?;
                        if subscribed { let _ = stopped.recv().await; }
                        Ok::<_, anyhow::Error>(())
                    };
                    let _ = operation.await;
                });
            }
        });
        Ok(Self { activity, stop, task, path })
    }
    fn disconnect(&self) { let _ = self.stop.send(()); }
}
fn no_dim_profile() -> PowerProfile {
    PowerProfile { dim_after: None, screen_off_after: None, lock_after: None, suspend_after: None,
        lid_closed: LidAction::Ignore, lid_closed_docked: LidAction::Ignore,
        lid_panel: PanelPolicy::Keep, lid_panel_docked: PanelPolicy::Keep }
}
fn actor(store: Store, session: zbus::Connection, system: zbus::Connection,
    edges: &SessionEdges, ready: tokio::sync::watch::Receiver<bool>, control: CompositorControl) -> Actor {
    let (_, commands) = mpsc::channel(16);
    let unavailable = || PowerFeature::unavailable("not observed");
    Actor { store, session, system: Some(system), commands, edges: edges.subscribe(), sleep_ready: ready,
        sleep_watch_alive: true, signals: SelectAll::new(), session_signals: SelectAll::new(), control: Some(control),
        activity_stream: None, activity: None, observed_at: Instant::now(),
        status: PowerObservation { revision: Revision::new_owner(), battery: None, mains: None,
            selected_source: None, selected_profile: None, session_active: None, inhibited: None, protected_covered: None,
            screen_off_after_ms: None, screen_off_observed: false, screen_off: unavailable(), dim: unavailable(),
            lock: unavailable(), suspend: unavailable(), hibernate: unavailable(), lid: unavailable(),
            lid_present: None, lid_closed: None, docked: None, lid_inhibitor_owned: false, sleep_delay_owned: false,
            dimmed_percent: None, sleep_pending: None, errors: Vec::new() },
        login_owner: None, upower_owner: None, lid_inhibitor: None, sleep_delay: None, delay_max: Duration::ZERO,
        dimmed: None, restore_requested: false, pending_sleep: None, dim_done: false, lock_done: false,
        suspend_done: false, policy_dirty: true, lid_applied: None, panel_owned: false, sleeping: false }
}
async fn assert_closed(proof: &mut PipeProof) -> Result<()> {
    ensure!(proof.what == "sleep", "unexpected pipe class");
    tokio::time::timeout(CALL_DEADLINE, &mut proof.closed).await.context("owned inhibitor did not close")??;
    Ok(())
}
async fn inspect_until(commands: &mpsc::Sender<Command>, accepted: impl Fn(&PowerObservation) -> bool) -> Result<PowerObservation> {
    tokio::time::timeout(CALL_DEADLINE, async {
        loop {
            let (reply, received) = oneshot::channel();
            commands.send(Command::Inspect(reply)).await.map_err(|_| anyhow::anyhow!("actual power owner stopped"))?;
            let observed: PowerObservation = serde_json::from_str(&received.await??)?;
            if accepted(&observed) { return Ok::<_, anyhow::Error>(observed); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?
}

#[tokio::test]
#[ignore = "requires an owned empty XDG_RUNTIME_DIR; run native-power-identity-proof/run.py"]
async fn native_private_power_owner_lifetimes_and_deadlines() -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("owned runtime directory required")?;
    let runtime = PathBuf::from(runtime);
    ensure!(std::env::var("TEMPLAR_NATIVE_POWER_FIXTURE").as_deref() == Ok("1")
        && runtime.join("owned-native-power-fixture").is_file()
        && !runtime.join(templar_types::compositor_control::CONTROL_SOCKET_NAME).exists(),
        "refusing public PowerApi constructor without an owned empty runtime fixture");
    let session_bus = PrivateBus::new().await?;
    let system_bus = PrivateBus::new().await?;
    let state = Arc::new(ProducerState::default());
    let login = system_bus.connect().await?;
    login.object_server().at(LOGIN_PATH, LoginProducer(state.clone())).await?;
    login.request_name(LOGIN).await?;
    let upower = system_bus.connect().await?;
    upower.object_server().at(UPOWER_PATH, UPowerProducer(state.clone())).await?;
    upower.request_name(UPOWER).await?;
    let session = session_bus.connect().await?;
    let client = session_bus.connect().await?;
    let store = Store::in_memory()?;
    let profile = no_dim_profile();
    store_profile(&store, PowerSource::Mains, Some(&profile))?;
    store_profile(&store, PowerSource::Battery, Some(&profile))?;
    let (edges, _) = broadcast::channel(crate::apis::lifecycle::EDGE_CAPACITY);
    let absent_watch = spawn_sleep_watch(None, edges.clone());
    let api = PowerApi::new(store.clone(), session.clone(), Some(system_bus.connect().await?), edges.clone(), absent_watch.readiness()).await;
    session.object_server().at(PATH, api).await?;
    let proxy = zbus::Proxy::new(&client, session.unique_name().unwrap().as_str(), PATH, "org.templaros.Power1").await?;
    let inspected: String = proxy.call("Inspect", &()).await?;
    let initial: PowerObservation = serde_json::from_str(&inspected)?;
    ensure!(initial.selected_source == Some(PowerSource::Mains) && initial.mains == Some(profile.clone()), "stored versus selected profile differs: {initial:?}");
    ensure!(!initial.suspend.armed && !initial.sleep_delay_owned && !initial.screen_off_observed && initial.session_active.is_none(), "unavailable native/witness state armed a physical effect");
    let mut edited = profile.clone(); edited.lock_after = Some(Duration::from_secs(77));
    let json = serde_json::to_string(&edited)?;
    proxy.call::<_, _, ()>("SetProfile", &("battery", &json)).await?;
    let stored: String = proxy.call("GetProfile", &("battery",)).await?;
    ensure!(serde_json::from_str::<PowerProfile>(&stored)? == edited, "inactive-source change was not stored");
    let result: zbus::Result<()> = proxy.call("SetProfile", &("mains", &json)).await;
    ensure!(result.is_err(), "selected profile without native confirmation reported applied success");
    let stored: String = proxy.call("GetProfile", &("mains",)).await?;
    ensure!(serde_json::from_str::<PowerProfile>(&stored)? == edited, "failed native effect discarded stored desire");
    proxy.call::<_, _, ()>("ResetProfile", &("battery",)).await?;
    let reset: String = proxy.call("GetProfile", &("battery",)).await?;
    ensure!(serde_json::from_str::<PowerProfile>(&reset)? == PowerProfile::default_for(PowerSource::Battery), "reset did not remove override");
    session.object_server().remove::<PowerApi, _>(PATH).await?;
    drop(absent_watch);
    store_profile(&store, PowerSource::Mains, Some(&profile))?;
    store_profile(&store, PowerSource::Battery, Some(&profile))?;

    // The production transport pins this actual Unix peer with SO_PEERPIDFD and must still
    // reject its non-compositor kernel cgroup. No fixture-labelled role bypass is accepted.
    let unmanaged = tokio::net::UnixListener::bind(runtime.join(templar_types::compositor_control::CONTROL_SOCKET_NAME))?;
    let rejected = CompositorControl::from_runtime()?.request(ControlRequest::Activity).await
        .err().context("unmanaged owned socket peer became compositor authority")?;
    ensure!(rejected.code == "owner_unauthenticated" && rejected.message.contains("not the managed templar-comp.service"),
        "kernel peer pin/cgroup refusal differed: {rejected}");
    drop(unmanaged);
    std::fs::remove_file(runtime.join(templar_types::compositor_control::CONTROL_SOCKET_NAME))?;

    let root = tempfile::tempdir()?;
    let socket = SocketFixture::new(root.path()).await?;
    let mut witness = Some(spawn_sleep_watch(Some(system_bus.connect().await?), edges.clone()));
    let mut ready = witness.as_ref().unwrap().readiness();
    tokio::time::timeout(CALL_DEADLINE, async {
        while !*ready.borrow_and_update() { ready.changed().await?; }
        Ok::<_, anyhow::Error>(())
    }).await??;
    let mut owner = actor(store.clone(), session.clone(), system_bus.connect().await?, &edges,
        ready.clone(), CompositorControl::from_socket(&socket.path));
    owner.load_profiles(); owner.reconcile().await;
    ensure!(owner.sleep_delay.is_some() && !owner.status.screen_off.armed, "active controlled owner did not retain delay FD");
    {
        let mut pipes = state.pipes.lock();
        ensure!(pipes.len() == 1 && pipes[0].closed.try_recv().is_err(), "producer pipe closed before lifecycle loss");
    }
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal)
        .sender(session.unique_name().unwrap().as_str())?.interface("org.templaros.Session.Lifecycle1")?.member("LockRequested")?.build();
    let mut locks = MessageStream::for_match_rule(rule, &client, Some(8)).await?;
    owner.begin_sleep(SleepKind::Suspend).await?;
    tokio::time::timeout(CALL_DEADLINE, locks.next()).await?.context("coverage barrier did not request a lock")??;
    ensure!(owner.pending_sleep.is_some() && owner.sleep_delay.is_some() && state.sleeps.load(Ordering::SeqCst) == 0, "uncovered current generation crossed sleep barrier");
    // An accounting Locked edge has no native frame authority.
    let _ = edges.send(SessionEdge::Locked);
    owner.finish_sleep().await?;
    ensure!(state.sleeps.load(Ordering::SeqCst) == 0, "saved Locked edge authorized sleep");
    owner.pending_sleep.as_mut().unwrap().deadline = Instant::now();
    owner.deadline().await;
    ensure!(owner.pending_sleep.is_none() && owner.sleep_delay.is_some() && owner.status.suspend.error.is_some(), "automatic barrier expiry released delay or issued sleep");

    login.emit_signal(None::<&str>, LOGIN_PATH, LOGIN_IFACE, "PrepareForSleep", &(true,)).await?;
    ensure!(tokio::time::timeout(CALL_DEADLINE, owner.edges.recv()).await?? == SessionEdge::Locked, "accounting edge disappeared");
    ensure!(tokio::time::timeout(CALL_DEADLINE, owner.edges.recv()).await?? == SessionEdge::Sleeping(true), "sole witness missed private sleep signal");
    owner.sleeping = true;
    owner.begin_sleep(SleepKind::External).await?;
    ensure!(owner.pending_sleep.is_some() && owner.sleep_delay.is_some(), "external uncovered sleep discarded delay ownership");
    owner.pending_sleep.as_mut().unwrap().deadline = Instant::now();
    socket.disconnect();
    owner.control = None;
    owner.native_unavailable("owned socket producer departed".into());
    owner.deadline().await;
    ensure!(owner.pending_sleep.as_ref().is_some_and(|pending| pending.expired) && owner.next_wake().is_none(), "expired external barrier kept a past-deadline retry wake after native loss");
    ensure!(owner.sleep_delay.is_some() && state.pipes.lock()[0].closed.try_recv().is_err(), "external deadline/native loss dropped retained delay FD");
    owner.control = Some(CompositorControl::from_socket(&socket.path));
    let (commands, received) = mpsc::channel(16);
    owner.commands = received;
    let task = tokio::spawn(async move { owner.run().await; owner });
    drop(witness.take());
    let mut proofs = std::mem::take(&mut *state.pipes.lock());
    assert_closed(&mut proofs[0]).await?;
    let (reply, received) = oneshot::channel();
    commands.send(Command::Inspect(reply)).await?;
    let observed: PowerObservation = serde_json::from_str(&received.await??)?;
    ensure!(!observed.sleep_delay_owned && observed.sleep_pending.is_none(), "sole-witness loss did not retire actual actor delay ownership");
    drop(commands);
    let mut owner = task.await?;
    ensure!(!owner.sleep_observed(), "retired sole witness stayed authoritative");
    ensure!(owner.begin_sleep(SleepKind::Suspend).await.is_err(), "absent sole witness admitted sleep");
    drop(owner);
    drop(socket);

    // Inactive seat and unavailable coverage are separate refusal inputs on a live owned socket.
    let socket = SocketFixture::new(root.path()).await?;
    let witness = spawn_sleep_watch(Some(system_bus.connect().await?), edges.clone());
    let mut ready = witness.readiness();
    tokio::time::timeout(CALL_DEADLINE, async { while !*ready.borrow_and_update() { ready.changed().await?; } Ok::<_, anyhow::Error>(()) }).await??;
    let mut owner = actor(store.clone(), session.clone(), system_bus.connect().await?, &edges, ready, CompositorControl::from_socket(&socket.path));
    owner.load_profiles();
    socket.activity.lock().session_active = false;
    owner.reconcile().await;
    ensure!(owner.sleep_delay.is_none() && owner.begin_sleep(SleepKind::Suspend).await.is_err(), "inactive seat retained automatic sleep authority");
    socket.activity.lock().session_active = true;
    socket.activity.lock().display_control = Capability::Unavailable { reason: "owned fixture coverage unavailable".into() };
    owner.reconcile().await;
    ensure!(owner.sleep_delay.is_none() && owner.begin_sleep(SleepKind::Suspend).await.is_err(), "unavailable coverage retained sleep authority");
    socket.activity.lock().display_control = Capability::Available { requires_restart: false };
    owner.reconcile().await;
    ensure!(owner.sleep_delay.is_some(), "owned producer recovery did not recover delay ownership");
    let mut recovery_proofs = std::mem::take(&mut *state.pipes.lock());
    let (commands, received) = mpsc::channel(16);
    owner.commands = received;
    let task = tokio::spawn(async move { owner.run().await; owner });
    upower.release_name(UPOWER).await?;
    let lost_source = inspect_until(&commands, |status| status.selected_source.is_none() && status.sleep_delay_owned).await?;
    ensure!(!lost_source.suspend.armed && !lost_source.lid.armed && recovery_proofs[0].closed.try_recv().is_err(),
        "UPower-only loss armed automatic policy or dropped independent external sleep guard");
    login.release_name(LOGIN).await?;
    assert_closed(&mut recovery_proofs[0]).await?;
    inspect_until(&commands, |status| !status.sleep_delay_owned && !status.suspend.armed).await?;
    drop(commands);
    let mut owner = task.await?;
    login.request_name(LOGIN).await?;
    owner.reconcile().await;
    ensure!(owner.status.selected_source.is_none() && owner.sleep_delay.is_some(), "healthy logind/native/witness could not reacquire guard without UPower");
    upower.request_name(UPOWER).await?;
    owner.reconcile().await;
    ensure!(owner.status.selected_source == Some(PowerSource::Mains), "power source producer recovery did not restore selected profile");
    let mut native_proofs = std::mem::take(&mut *state.pipes.lock());
    owner.native_unavailable("owned native producer loss outside external barrier".into());
    assert_closed(&mut native_proofs[0]).await?;
    owner.reconcile().await;
    ensure!(owner.sleep_delay.is_some(), "native recovery did not recover delay ownership");
    let mut final_proofs = std::mem::take(&mut *state.pipes.lock());
    drop(owner);
    assert_closed(&mut final_proofs[0]).await?;
    drop(witness); drop(socket);

    // Actual BacklightApi and actual logind writer, confined to this owned directory.
    let sys = root.path().join("backlight");
    std::fs::create_dir_all(sys.join("owned-panel"))?;
    std::fs::write(sys.join("owned-panel/max_brightness"), "100")?;
    std::fs::write(sys.join("owned-panel/brightness"), "80")?;
    login.object_server().at("/org/freedesktop/login1/session/auto", BrightnessProducer { state: state.clone(), root: sys.clone() }).await?;
    session.object_server().at(templar_types::dbus::session::BACKLIGHT_PATH, BacklightApi::new(SysfsBacklight::new(&sys), Arc::new(LogindBrightness::new(Some(system_bus.connect().await?))))).await?;
    let witness = spawn_sleep_watch(None, edges.clone());
    let mut owner = actor(store, session.clone(), system_bus.connect().await?, &edges, witness.readiness(), CompositorControl::from_socket(root.path().join("never-connected.sock")));
    owner.dim().await?;
    ensure!(std::fs::read_to_string(sys.join("owned-panel/brightness"))? == "40" && owner.dimmed.is_some(), "actual dim target/ownership differed");
    owner.restore_dim().await?;
    ensure!(std::fs::read_to_string(sys.join("owned-panel/brightness"))? == "80" && owner.dimmed.is_none(), "unchanged owned raw target did not restore");
    owner.dim().await?;
    std::fs::write(sys.join("owned-panel/brightness"), "65")?;
    let writes = state.brightness_writes.load(Ordering::SeqCst);
    owner.restore_dim().await?;
    ensure!(std::fs::read_to_string(sys.join("owned-panel/brightness"))? == "65" && state.brightness_writes.load(Ordering::SeqCst) == writes && owner.dimmed.is_none(), "external raw writer was overwritten by stale dim ownership");
    ensure!(state.sleeps.load(Ordering::SeqCst) == 0, "a negative barrier case reached a physical-effect method");
    drop(owner); drop(witness); drop(proxy); drop(client); drop(session); drop(login); drop(upower);
    session_bus.stop().await?; system_bus.stop().await?;
    println!("PASS actual PowerApi stored/selected/reset distinction; sole SleepWatch; negative coverage/inactive barriers; expired external delay retained without retry spin; UPower-only loss keeps external sleep guard, login1/native/witness/lifetime loss closes actual pipe; conditional dim restore on owned sysfs via actual BacklightApi/logind writer. Socket coverage and CanSuspend are controlled inputs, no KMS/physical authorization/suspend claim.");
    Ok(())
}
