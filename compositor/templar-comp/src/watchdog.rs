//! Evidence for a frozen display: is the event loop stuck, and if not, is the display?
//!
//! A freeze that leaves the rest of the machine running can be one of four quite different faults —
//! the render thread deadlocked, busy in a loop, blocked in a DRM/GPU ioctl, or alive and waiting
//! for a page flip the kernel never completes — and from the outside all four look the same: no
//! pixels and no log lines. This module makes them tell themselves apart in the journal.
//!
//! **Event-loop stalls** are detected by a probe, not by counting iterations: `event_loop.run`
//! legitimately sleeps forever when nothing happens, so "no iteration for a while" is the normal
//! idle state. A thread named `comp-watchdog` pings a calloop [`PingSource`] registered on the main
//! loop once per [`Config::probe_interval`]; the handler only stores two atomics. A probe left
//! unanswered for [`Config::stall_threshold`] is a stall, reported with the main thread's kernel
//! side (`/proc/self/task/<tid>/{stat,wchan,syscall}`) and every other thread that is not asleep,
//! and repeated on a backoff schedule ([`reports_passed`]) while it lasts. The answer that finally
//! arrives is reported once as a recovery. The probe costs the main loop one wakeup per interval
//! and never a lock; the watchdog never waits on the main thread for anything.
//!
//! **Overdue page flips** are the case where the loop is fine and the display is not. The udev
//! backend tells a [`FlipHandle`] per CRTC when a frame is queued for scanout and when its
//! completion arrives; the watchdog reads those atomics and reports a flip still outstanding after
//! [`Config::flip_overdue`]. Blanking an output (`DrmCompositor::clear`) and pausing the session for
//! a VT switch [`FlipHandle::cancel`] the outstanding flip, because neither is going to complete
//! one and neither is a fault.
//! The udev main-loop repaint watchdog safely re-arms overdue flips by synchronously clearing
//! scanout before releasing pending state; this thread never touches DRM or fabricates completion.
//!
//! Optionally (`TEMPLAR_COMP_WATCHDOG_ABORT_AFTER_SECS`) a stall that long aborts the process, so a
//! core dump records every thread's stack — the evidence `/proc` cannot give.
//!
//! The decisions ([`reports_passed`], [`FlipWatch::observe`], [`parse_abort_after`]) and the
//! `/proc` parsing are pure and tested below; so is the whole thing, against a real calloop loop.

use smithay::reexports::calloop::{
    LoopHandle,
    ping::{Ping, PingSource, make_ping},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, PoisonError, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::Thread,
    time::{Duration, Instant},
};

/// The environment variable that turns a long enough stall into an abort.
pub const ABORT_ENV: &str = "TEMPLAR_COMP_WATCHDOG_ABORT_AFTER_SECS";

/// Report schedule as multiples of the threshold: with a 5 s stall threshold, reports at 5, 15, 30
/// and 60 s, then every 60 s for as long as the condition lasts.
const BACKOFF_MULTIPLES: [u32; 4] = [1, 3, 6, 12];

/// Timing for one watchdog. [`Config::production`] is what the compositor runs; tests inject
/// short intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// How often the main loop is probed — and so the most often the probe wakes it.
    pub probe_interval: Duration,
    /// How long a probe may go unanswered before the loop counts as stalled.
    pub stall_threshold: Duration,
    /// How long a queued page flip may stay outstanding before it counts as overdue.
    pub flip_overdue: Duration,
    /// The watchdog's own re-check period while a probe or a flip is outstanding. With nothing
    /// outstanding it sleeps until the next probe is due.
    pub tick: Duration,
    /// Abort the process once a stall lasts this long; `None` is never.
    pub abort_after: Option<Duration>,
}

impl Config {
    pub fn production(abort_after: Option<Duration>) -> Self {
        Self {
            probe_interval: Duration::from_secs(5),
            stall_threshold: Duration::from_secs(5),
            flip_overdue: Duration::from_secs(2),
            tick: Duration::from_secs(1),
            abort_after,
        }
    }
}

/// Something the watchdog observed. The compositor logs these ([`log_report`]); tests collect them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// The probe has been unanswered for `stalled` (a lower bound on how long the loop has been
    /// stuck: it may have stopped any time after it last answered).
    Stalled {
        stalled: Duration,
        main: ThreadState,
        /// `name:state:wchan` for every thread not asleep in `S`, comma-separated, or `none`.
        other_threads: String,
        /// The oldest outstanding page flip's age, if one is outstanding.
        flip_pending: Option<Duration>,
    },
    /// The probe was answered after a reported stall; `stalled` is how long it took.
    Recovered {
        stalled: Duration,
    },
    FlipOverdue {
        crtc: String,
        queued_ago: Duration,
    },
    FlipCompletedLate {
        crtc: String,
        after: Duration,
    },
    Aborting {
        stalled: Duration,
        abort_after: Duration,
    },
}

/// Where reports go: [`log_report`] in the compositor, a channel in tests.
pub type Reporter = Arc<dyn Fn(&Report) + Send + Sync>;

/// The main loop's kernel-side state, as `/proc` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadState {
    /// The one-letter state from `stat` (`R`, `S`, `D`, ...), `?` if it could not be read.
    pub state: char,
    pub wchan: String,
    /// [`SyscallState::describe`], or `?` if it could not be read.
    pub syscall: String,
}

/// How many reports a condition that has lasted `elapsed` is owed, given its `threshold`: none
/// before the threshold, then one more at each of 1×, 3×, 6× and 12× it, then one per further 12×.
///
/// The caller reports when this exceeds the count it has already made and then adopts it, so a
/// watchdog that was itself delayed past several points reports once rather than in a burst.
pub fn reports_passed(elapsed: Duration, threshold: Duration) -> u32 {
    if threshold.is_zero() || elapsed < threshold {
        return 0;
    }
    let mut passed = 0;
    for multiple in BACKOFF_MULTIPLES {
        if elapsed < threshold * multiple {
            return passed;
        }
        passed += 1;
    }
    let period = threshold * BACKOFF_MULTIPLES[BACKOFF_MULTIPLES.len() - 1];
    let beyond = elapsed.saturating_sub(period);
    let extra = beyond.as_nanos() / period.as_nanos();
    passed.saturating_add(u32::try_from(extra).unwrap_or(u32::MAX))
}

/// `TEMPLAR_COMP_WATCHDOG_ABORT_AFTER_SECS`: unset, empty or `0` is never; a positive whole number
/// of seconds is that; anything else is an error the caller logs once and treats as never.
pub fn parse_abort_after(raw: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    match raw.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(seconds) => Ok(Some(Duration::from_secs(seconds))),
        Err(error) => Err(format!(
            "{raw:?} is not a whole number of seconds ({error})"
        )),
    }
}

/// The comm name and one-letter state from a `/proc/<pid>/task/<tid>/stat` line.
///
/// The comm is everything between the first `(` and the *last* `)`, because a thread may name
/// itself `a) b (c` and the kernel does not escape it.
pub fn parse_stat(contents: &str) -> Option<(String, char)> {
    let open = contents.find('(')?;
    let close = contents.rfind(')')?;
    if close < open {
        return None;
    }
    let name = contents[open + 1..close].to_owned();
    let state = contents[close + 1..].trim_start().chars().next()?;
    Some((name, state))
}

/// What `/proc/<pid>/task/<tid>/syscall` says the thread is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyscallState {
    /// On a CPU right now; the kernel cannot say more.
    Running,
    /// Blocked, but not inside a system call (`-1`).
    NotInSyscall,
    /// Inside system call `nr`; the first two arguments are kept (an ioctl's fd and request).
    InSyscall { nr: u64, args: [u64; 2] },
}

impl SyscallState {
    pub fn parse(contents: &str) -> Option<Self> {
        let mut fields = contents.split_whitespace();
        let first = fields.next()?;
        if first == "running" {
            return Some(Self::Running);
        }
        if first == "-1" {
            return Some(Self::NotInSyscall);
        }
        let nr = first.parse().ok()?;
        let mut args = [0; 2];
        for arg in &mut args {
            *arg = fields
                .next()
                .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
        }
        Some(Self::InSyscall { nr, args })
    }

    /// `running`, `none`, the syscall's name (an ioctl with its fd and request, which is what
    /// tells a DRM atomic commit from a GEM wait), or the raw number for one not in the table.
    pub fn describe(&self) -> String {
        match self {
            Self::Running => "running".to_owned(),
            Self::NotInSyscall => "none".to_owned(),
            Self::InSyscall { nr, args } => match syscall_name(*nr) {
                Some("ioctl") => format!("ioctl(fd={},req={:#x})", args[0], args[1]),
                Some(name) => name.to_owned(),
                None => nr.to_string(),
            },
        }
    }
}

/// The x86_64 numbers for the system calls a stuck compositor thread is plausibly sitting in.
#[cfg(target_arch = "x86_64")]
fn syscall_name(nr: u64) -> Option<&'static str> {
    Some(match nr {
        0 => "read",
        1 => "write",
        3 => "close",
        7 => "poll",
        9 => "mmap",
        11 => "munmap",
        16 => "ioctl",
        17 => "pread64",
        19 => "readv",
        20 => "writev",
        23 => "select",
        24 => "sched_yield",
        35 => "nanosleep",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        61 => "wait4",
        202 => "futex",
        230 => "clock_nanosleep",
        232 => "epoll_wait",
        257 => "openat",
        270 => "pselect6",
        271 => "ppoll",
        281 => "epoll_pwait",
        426 => "io_uring_enter",
        441 => "epoll_pwait2",
        _ => return None,
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn syscall_name(_nr: u64) -> Option<&'static str> {
    None
}

/// This thread's kernel id, from `/proc/thread-self` (`<pid>/task/<tid>`) — no `unsafe` needed.
pub fn current_tid() -> Option<u32> {
    let link = std::fs::read_link("/proc/thread-self").ok()?;
    link.file_name()?.to_str()?.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_owned())
}

fn read_main_thread(tid: u32) -> ThreadState {
    let dir = Path::new("/proc/self/task").join(tid.to_string());
    let state = read_trimmed(&dir.join("stat"))
        .and_then(|stat| parse_stat(&stat))
        .map_or('?', |(_, state)| state);
    let wchan = read_trimmed(&dir.join("wchan")).unwrap_or_else(|| "?".to_owned());
    let syscall = read_trimmed(&dir.join("syscall"))
        .and_then(|contents| SyscallState::parse(&contents))
        .map_or_else(|| "?".to_owned(), |syscall| syscall.describe());
    ThreadState {
        state,
        wchan,
        syscall,
    }
}

/// `name:state:wchan` for every thread of this process not asleep in `S` (or `I`), except the two
/// the report already accounts for: the main loop's and the watchdog's own.
fn other_busy_threads(skip: &[u32]) -> String {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return "?".to_owned();
    };
    let mut busy = Vec::new();
    for task in tasks.flatten() {
        let Some(tid) = task.file_name().to_str().and_then(|tid| tid.parse().ok()) else {
            continue;
        };
        if skip.contains(&tid) {
            continue;
        }
        let dir = task.path();
        let Some((name, state)) =
            read_trimmed(&dir.join("stat")).and_then(|stat| parse_stat(&stat))
        else {
            continue;
        };
        if matches!(state, 'S' | 'I') {
            continue;
        }
        let wchan = read_trimmed(&dir.join("wchan")).unwrap_or_else(|| "?".to_owned());
        busy.push(format!("{name}:{state}:{wchan}"));
    }
    if busy.is_empty() {
        "none".to_owned()
    } else {
        busy.join(",")
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What the main thread and the watchdog share. Everything the main thread touches per frame is
/// an atomic; the one mutex guards the list of CRTCs, which the main thread takes only when an
/// output appears and the watchdog only long enough to clone it.
struct Shared {
    epoch: Instant,
    /// The sequence number of the probe most recently sent (written by the watchdog).
    sent_seq: AtomicU64,
    /// The sequence number most recently answered, and when (ms since `epoch`), written by the
    /// main loop's ping handler.
    answered_seq: AtomicU64,
    answered_at_ms: AtomicU64,
    flips: Mutex<Vec<Weak<FlipSlot>>>,
    /// Set while the watchdog sleeps until the next probe rather than for one tick, so a flip
    /// queued meanwhile wakes it (once) instead of going unwatched until then.
    dozing: AtomicBool,
    thread: OnceLock<Thread>,
    stop: AtomicBool,
    reporter: Reporter,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        millis(self.epoch.elapsed())
    }

    fn wake_watchdog(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

/// One CRTC's outstanding flip. `queued_at` is ms since the epoch plus one, so zero is "none".
struct FlipSlot {
    label: String,
    queued_at: AtomicU64,
    /// The `queued_at` value the watchdog last reported overdue, so completion knows it was late.
    warned_for: AtomicU64,
}

/// The main thread's side of one CRTC's flip tracking. Dropping it (the output going away) stops
/// the watchdog watching that CRTC.
pub struct FlipHandle {
    slot: Arc<FlipSlot>,
    shared: Arc<Shared>,
}

impl FlipHandle {
    /// A frame was queued for scanout. A flip already outstanding keeps its older timestamp: a
    /// frame queued behind it does not make the stuck one any younger.
    pub fn queued(&self) {
        let stamp = self.shared.now_ms().saturating_add(1);
        if self
            .slot
            .queued_at
            .compare_exchange(0, stamp, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            && self.shared.dozing.swap(false, Ordering::SeqCst)
        {
            self.shared.wake_watchdog();
        }
    }

    /// The flip completed. Reports it if the watchdog had already called it overdue.
    pub fn completed(&self) {
        let queued_at = self.slot.queued_at.swap(0, Ordering::SeqCst);
        if queued_at != 0 && self.slot.warned_for.load(Ordering::SeqCst) == queued_at {
            let after = self.shared.now_ms().saturating_sub(queued_at - 1);
            (self.shared.reporter)(&Report::FlipCompletedLate {
                crtc: self.slot.label.clone(),
                after: Duration::from_millis(after),
            });
        }
    }

    /// Nothing outstanding is going to complete, and that is not a fault: the output was blanked
    /// (`DrmCompositor::clear`) or the session paused for a VT switch.
    pub fn cancel(&self) {
        self.slot.queued_at.store(0, Ordering::SeqCst);
    }
}

/// The watchdog's per-CRTC memory: which flip it is looking at and how often it has reported it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlipWatch {
    queued_at: u64,
    reports: u32,
}

impl FlipWatch {
    /// Look at a CRTC's `queued_at` (ms + 1, zero for none) at `now_ms`; returns how long ago the
    /// flip was queued when a report is due — once at the threshold, then on the backoff schedule —
    /// and `None` otherwise. A different flip starts the count again.
    pub fn observe(
        &mut self,
        queued_at: u64,
        now_ms: u64,
        threshold: Duration,
    ) -> Option<Duration> {
        if queued_at != self.queued_at {
            *self = Self {
                queued_at,
                reports: 0,
            };
        }
        if queued_at == 0 {
            return None;
        }
        let age = Duration::from_millis(now_ms.saturating_sub(queued_at - 1));
        let due = reports_passed(age, threshold);
        (due > self.reports).then(|| {
            self.reports = due;
            age
        })
    }
}

/// The main thread's handle on a running watchdog. Dropping it stops the thread.
pub struct Watchdog {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Watchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watchdog").finish_non_exhaustive()
    }
}

impl Watchdog {
    /// Start watching the CRTC `label` names.
    pub fn track(&self, label: String) -> FlipHandle {
        let slot = Arc::new(FlipSlot {
            label,
            queued_at: AtomicU64::new(0),
            warned_for: AtomicU64::new(0),
        });
        self.shared
            .flips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&slot));
        FlipHandle {
            slot,
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake_watchdog();
    }
}

/// Register the probe on `handle`'s loop, whose thread is `loop_tid`, and start the watchdog
/// thread.
pub fn install<D>(
    handle: &LoopHandle<'_, D>,
    config: Config,
    loop_tid: u32,
    reporter: Reporter,
) -> std::io::Result<Watchdog> {
    let (ping, source): (Ping, PingSource) = make_ping()?;
    let shared = Arc::new(Shared {
        epoch: Instant::now(),
        sent_seq: AtomicU64::new(0),
        answered_seq: AtomicU64::new(0),
        answered_at_ms: AtomicU64::new(0),
        flips: Mutex::new(Vec::new()),
        dozing: AtomicBool::new(false),
        thread: OnceLock::new(),
        stop: AtomicBool::new(false),
        reporter,
    });
    let answering = Arc::clone(&shared);
    handle
        .insert_source(source, move |(), &mut (), _| {
            answering
                .answered_at_ms
                .store(answering.now_ms(), Ordering::SeqCst);
            answering
                .answered_seq
                .store(answering.sent_seq.load(Ordering::SeqCst), Ordering::SeqCst);
        })
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let watching = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("comp-watchdog".to_owned())
        .spawn(move || watch(&watching, &ping, config, loop_tid))?;
    let _ = shared.thread.set(thread.thread().clone());
    Ok(Watchdog { shared })
}

/// The compositor's watchdog for the calling (main) thread's loop: reads the abort setting, logs
/// that it is armed, and reports through [`log_report`]. A watchdog that cannot start is logged
/// and skipped — it is evidence, never a reason not to run.
pub fn install_for_main_loop<D>(handle: &LoopHandle<'_, D>) -> Option<Watchdog> {
    let raw = std::env::var(ABORT_ENV).ok();
    let abort_after = parse_abort_after(raw.as_deref()).unwrap_or_else(|error| {
        tracing::warn!(%error, variable = ABORT_ENV, "ignoring the watchdog abort setting; never aborting");
        None
    });
    let config = Config::production(abort_after);
    // The main thread's tid is the process id; `run` is called on it.
    match install(handle, config, std::process::id(), Arc::new(log_report)) {
        Ok(watchdog) => {
            tracing::info!(
                probe_interval_ms = millis(config.probe_interval),
                stall_threshold_ms = millis(config.stall_threshold),
                flip_overdue_ms = millis(config.flip_overdue),
                abort_after_secs = abort_after.map_or(0, |after| after.as_secs()),
                "compositor watchdog armed"
            );
            Some(watchdog)
        }
        Err(error) => {
            tracing::warn!(%error, "could not start the compositor watchdog; running without one");
            None
        }
    }
}

/// The log lines `templar doctor freeze` looks for (target `templar_comp::watchdog`).
pub fn log_report(report: &Report) {
    match report {
        Report::Stalled {
            stalled,
            main,
            other_threads,
            flip_pending,
        } => tracing::warn!(
            stalled_ms = millis(*stalled),
            main_state = %main.state,
            main_wchan = %main.wchan,
            main_syscall = %main.syscall,
            other_threads = %other_threads,
            flip_pending_ms = flip_pending.map(millis),
            "compositor event loop stalled"
        ),
        Report::Recovered { stalled } => tracing::info!(
            stalled_ms = millis(*stalled),
            "compositor event loop recovered"
        ),
        Report::FlipOverdue { crtc, queued_ago } => tracing::warn!(
            crtc = %crtc,
            queued_ms_ago = millis(*queued_ago),
            "page flip overdue"
        ),
        Report::FlipCompletedLate { crtc, after } => tracing::info!(
            crtc = %crtc,
            after_ms = millis(*after),
            "overdue page flip completed"
        ),
        Report::Aborting {
            stalled,
            abort_after,
        } => tracing::error!(
            stalled_ms = millis(*stalled),
            abort_after_secs = abort_after.as_secs(),
            "compositor event loop stalled past the abort threshold; aborting so systemd-coredump records every thread"
        ),
    }
}

struct Probe {
    seq: u64,
    sent: Instant,
    sent_ms: u64,
    reports: u32,
}

/// The watchdog thread.
fn watch(shared: &Shared, ping: &Ping, config: Config, loop_tid: u32) {
    let own_tid = current_tid();
    let mut probe: Option<Probe> = None;
    let mut next_probe = Instant::now() + config.probe_interval;
    let mut seq = 0_u64;
    let mut flip_watches: HashMap<usize, FlipWatch> = HashMap::new();

    while !shared.stop.load(Ordering::SeqCst) {
        let now = Instant::now();

        if let Some(outstanding) = probe
            .take_if(|outstanding| shared.answered_seq.load(Ordering::SeqCst) == outstanding.seq)
        {
            if outstanding.reports > 0 {
                let answered_ms = shared.answered_at_ms.load(Ordering::SeqCst);
                (shared.reporter)(&Report::Recovered {
                    stalled: Duration::from_millis(answered_ms.saturating_sub(outstanding.sent_ms)),
                });
            }
            next_probe = outstanding.sent + config.probe_interval;
        }

        if probe.is_none() && now >= next_probe {
            seq += 1;
            shared.sent_seq.store(seq, Ordering::SeqCst);
            ping.ping();
            probe = Some(Probe {
                seq,
                sent: now,
                sent_ms: shared.now_ms(),
                reports: 0,
            });
        }

        let slots = live_slots(shared);
        let now_ms = shared.now_ms();

        if let Some(outstanding) = probe.as_mut() {
            let stalled = now.saturating_duration_since(outstanding.sent);
            let due = reports_passed(stalled, config.stall_threshold);
            if due > outstanding.reports {
                outstanding.reports = due;
                let skip: Vec<u32> = std::iter::once(loop_tid).chain(own_tid).collect();
                let flip_pending = slots
                    .iter()
                    .map(|(_, slot)| slot.queued_at.load(Ordering::SeqCst))
                    .filter(|queued_at| *queued_at != 0)
                    .min()
                    .map(|queued_at| Duration::from_millis(now_ms.saturating_sub(queued_at - 1)));
                (shared.reporter)(&Report::Stalled {
                    stalled,
                    main: read_main_thread(loop_tid),
                    other_threads: other_busy_threads(&skip),
                    flip_pending,
                });
            }
            if let Some(abort_after) = config.abort_after
                && stalled >= abort_after
            {
                (shared.reporter)(&Report::Aborting {
                    stalled,
                    abort_after,
                });
                std::process::abort();
            }
        }

        flip_watches.retain(|id, _| slots.iter().any(|(slot_id, _)| slot_id == id));
        for (id, slot) in &slots {
            let queued_at = slot.queued_at.load(Ordering::SeqCst);
            if let Some(queued_ago) =
                flip_watches
                    .entry(*id)
                    .or_default()
                    .observe(queued_at, now_ms, config.flip_overdue)
            {
                slot.warned_for.store(queued_at, Ordering::SeqCst);
                (shared.reporter)(&Report::FlipOverdue {
                    crtc: slot.label.clone(),
                    queued_ago,
                });
            }
        }

        // Tick while anything is outstanding; otherwise sleep until the next probe, announcing it
        // first so a flip queued from here on wakes this thread. Announce, then look: a flip
        // queued in between is either seen below or sees `dozing`.
        shared.dozing.store(true, Ordering::SeqCst);
        let flip_outstanding = slots
            .iter()
            .any(|(_, slot)| slot.queued_at.load(Ordering::SeqCst) != 0);
        let wait = if probe.is_some() || flip_outstanding {
            shared.dozing.store(false, Ordering::SeqCst);
            config.tick
        } else {
            next_probe.saturating_duration_since(Instant::now())
        };
        drop(slots);
        std::thread::park_timeout(wait);
        shared.dozing.store(false, Ordering::SeqCst);
    }
}

/// The CRTCs still being tracked, each with a stable id (its slot's address), pruning any whose
/// output has gone. The lock is held only to clone the list.
fn live_slots(shared: &Shared) -> Vec<(usize, Arc<FlipSlot>)> {
    let mut flips = shared.flips.lock().unwrap_or_else(PoisonError::into_inner);
    flips.retain(|slot| slot.strong_count() > 0);
    flips
        .iter()
        .filter_map(Weak::upgrade)
        .map(|slot| (Arc::as_ptr(&slot) as usize, slot))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::calloop::{EventLoop, channel};
    use std::sync::mpsc;

    const fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// With the production 5 s threshold: nothing before 5 s, then 5, 15, 30, 60, then every 60.
    #[test]
    fn stall_reports_back_off_to_once_a_minute() {
        let threshold = secs(5);
        let schedule: Vec<(u64, u32)> = [0, 4, 5, 14, 15, 29, 30, 59, 60, 119, 120, 180, 600]
            .into_iter()
            .map(|at| (at, reports_passed(secs(at), threshold)))
            .collect();
        assert_eq!(
            schedule,
            vec![
                (0, 0),
                (4, 0),
                (5, 1),
                (14, 1),
                (15, 2),
                (29, 2),
                (30, 3),
                (59, 3),
                (60, 4),
                (119, 4),
                (120, 5),
                (180, 6),
                (600, 13)
            ]
        );
        assert_eq!(
            reports_passed(Duration::from_millis(4_999), threshold),
            0,
            "the threshold is a boundary, not a rounding"
        );
        assert_eq!(reports_passed(secs(100), Duration::ZERO), 0);
    }

    #[test]
    fn the_abort_setting_is_off_unless_it_is_a_positive_number_of_seconds() {
        assert_eq!(parse_abort_after(None), Ok(None));
        assert_eq!(parse_abort_after(Some("")), Ok(None));
        assert_eq!(parse_abort_after(Some("  ")), Ok(None));
        assert_eq!(parse_abort_after(Some("0")), Ok(None));
        assert_eq!(parse_abort_after(Some("120")), Ok(Some(secs(120))));
        assert_eq!(parse_abort_after(Some(" 30\n")), Ok(Some(secs(30))));
        assert!(parse_abort_after(Some("2m")).is_err());
        assert!(parse_abort_after(Some("-5")).is_err());
    }

    /// The comm is whatever sits between the first `(` and the last `)` — a thread may put both
    /// in its own name, and spaces too.
    #[test]
    fn stat_lines_parse_whatever_the_thread_called_itself() {
        assert_eq!(
            parse_stat("1234 (templar-comp) S 1 1234 1234 0 -1 4194560"),
            Some(("templar-comp".to_owned(), 'S'))
        );
        assert_eq!(
            parse_stat("77 (a) b (c) D 1 2 3"),
            Some(("a) b (c".to_owned(), 'D'))
        );
        assert_eq!(
            parse_stat("78 (tokio worker 1) R 1"),
            Some(("tokio worker 1".to_owned(), 'R'))
        );
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (trailing)"), None);
    }

    #[test]
    fn syscall_files_parse_into_what_the_thread_is_doing() {
        assert_eq!(SyscallState::parse("running"), Some(SyscallState::Running));
        assert_eq!(
            SyscallState::parse("-1 0x7ffd5e4a8f48 0x7f1c2a0e6d3e"),
            Some(SyscallState::NotInSyscall)
        );
        let ioctl = SyscallState::parse(
            "16 0xc 0xc03864bc 0x7ffd5e4a9000 0x0 0x0 0x0 0x7ffd5e4a8f48 0x7f1c2a0e6d3e",
        )
        .unwrap();
        assert_eq!(
            ioctl,
            SyscallState::InSyscall {
                nr: 16,
                args: [12, 0xc038_64bc]
            }
        );
        assert_eq!(SyscallState::parse(""), None);
        assert_eq!(SyscallState::parse("nonsense 0x1"), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn syscalls_are_named_where_known_and_numbered_where_not() {
        let named = |nr| SyscallState::InSyscall { nr, args: [0, 0] }.describe();
        assert_eq!(named(202), "futex");
        assert_eq!(named(232), "epoll_wait");
        assert_eq!(named(281), "epoll_pwait");
        assert_eq!(named(230), "clock_nanosleep");
        assert_eq!(named(47), "recvmsg");
        assert_eq!(named(999), "999");
        assert_eq!(
            SyscallState::InSyscall {
                nr: 16,
                args: [12, 0xc038_64bc]
            }
            .describe(),
            "ioctl(fd=12,req=0xc03864bc)"
        );
        assert_eq!(SyscallState::Running.describe(), "running");
        assert_eq!(SyscallState::NotInSyscall.describe(), "none");
    }

    /// Overdue at the threshold, once, then on the backoff schedule; a new flip starts again, and
    /// a cancelled one (blanked, paused) is never overdue.
    #[test]
    fn a_flip_is_overdue_once_at_the_threshold_and_again_only_on_the_backoff() {
        let threshold = secs(2);
        let mut watch = FlipWatch::default();
        let queued_at = 10_001; // queued at 10 000 ms
        assert_eq!(watch.observe(queued_at, 11_999, threshold), None);
        assert_eq!(
            watch.observe(queued_at, 12_000, threshold),
            Some(Duration::from_millis(2_000))
        );
        assert_eq!(watch.observe(queued_at, 13_000, threshold), None);
        assert_eq!(watch.observe(queued_at, 15_999, threshold), None);
        assert_eq!(
            watch.observe(queued_at, 16_000, threshold),
            Some(Duration::from_millis(6_000))
        );

        // The next flip is a new flip, however late the last one was.
        assert_eq!(watch.observe(20_001, 21_000, threshold), None);
        assert_eq!(
            watch.observe(20_001, 22_500, threshold),
            Some(Duration::from_millis(2_500))
        );

        // Cancelled: nothing outstanding, however long ago anything was queued.
        let mut watch = FlipWatch::default();
        assert_eq!(watch.observe(0, 1_000_000, threshold), None);
    }

    fn fast_config() -> Config {
        Config {
            probe_interval: Duration::from_millis(50),
            stall_threshold: Duration::from_millis(200),
            flip_overdue: Duration::from_millis(100),
            tick: Duration::from_millis(10),
            abort_after: None,
        }
    }

    fn collecting() -> (Reporter, mpsc::Receiver<Report>) {
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        let reporter: Reporter = Arc::new(move |report: &Report| {
            let _ = sender
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(report.clone());
        });
        (reporter, receiver)
    }

    fn next(receiver: &mpsc::Receiver<Report>) -> Report {
        receiver
            .recv_timeout(secs(5))
            .expect("the watchdog reports within the deadline")
    }

    /// A real calloop loop on its own thread, idle (which must not count) and then blocked in a
    /// sleep: the watchdog reports the stall with the loop thread's real kernel state, and then the
    /// recovery once, when the probe is answered.
    #[test]
    fn a_blocked_loop_is_reported_stalled_and_then_recovered() {
        let (reporter, reports) = collecting();
        let (block_sender, block_source) = channel::channel::<Duration>();
        let (ready_sender, ready) = mpsc::channel();
        let loop_thread = std::thread::spawn(move || {
            let mut event_loop: EventLoop<()> = EventLoop::try_new().unwrap();
            let signal = event_loop.get_signal();
            event_loop
                .handle()
                .insert_source(block_source, move |event, (), ()| match event {
                    channel::Event::Msg(duration) => std::thread::sleep(duration),
                    channel::Event::Closed => signal.stop(),
                })
                .unwrap();
            let watchdog = install(
                &event_loop.handle(),
                fast_config(),
                current_tid().unwrap(),
                reporter,
            )
            .unwrap();
            ready_sender.send(()).unwrap();
            event_loop.run(None, &mut (), |()| {}).unwrap();
            drop(watchdog);
        });
        ready.recv().unwrap();

        // Idle for several probe intervals: an idle loop answers its probes and is not stalled.
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            reports.try_recv().is_err(),
            "an idle loop is not a stalled one"
        );

        block_sender.send(Duration::from_millis(800)).unwrap();
        let Report::Stalled {
            stalled,
            main,
            flip_pending,
            ..
        } = next(&reports)
        else {
            panic!("the first report is the stall");
        };
        assert!(stalled >= fast_config().stall_threshold);
        assert_eq!(main.state, 'S', "the loop thread is asleep in the kernel");
        assert!(
            ["nanosleep", "clock_nanosleep"].contains(&main.syscall.as_str()),
            "the loop thread is reported in its sleep, got {:?}",
            main.syscall
        );
        assert_eq!(flip_pending, None);

        let recovered = loop {
            match next(&reports) {
                Report::Stalled { .. } => {}
                Report::Recovered { stalled } => break stalled,
                other => panic!("unexpected report {other:?}"),
            }
        };
        assert!(recovered >= fast_config().stall_threshold);
        assert!(recovered < secs(2));

        // Answered and recovered: quiet again.
        std::thread::sleep(Duration::from_millis(200));
        assert!(reports.try_recv().is_err(), "recovery is reported once");

        drop(block_sender);
        loop_thread.join().unwrap();
    }

    /// A flip queued and never completed is overdue; completing it late says so; a cancelled one
    /// (an idle blank, a VT switch) is never reported at all.
    #[test]
    fn an_outstanding_flip_is_overdue_and_a_cancelled_one_is_not() {
        let (reporter, reports) = collecting();
        let event_loop: EventLoop<()> = EventLoop::try_new().unwrap();
        // The loop is never dispatched, so give the probe all the time in the world: this test is
        // about flips alone.
        let config = Config {
            probe_interval: secs(3600),
            ..fast_config()
        };
        let watchdog = install(
            &event_loop.handle(),
            config,
            current_tid().unwrap(),
            reporter,
        )
        .unwrap();
        let flip = watchdog.track("eDP-1:80".to_owned());

        flip.queued();
        flip.cancel();
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            reports.try_recv().is_err(),
            "a cancelled flip is never overdue"
        );
        flip.completed();
        assert!(reports.try_recv().is_err(), "nor completed late");

        flip.queued();
        let Report::FlipOverdue { crtc, queued_ago } = next(&reports) else {
            panic!("an outstanding flip is overdue");
        };
        assert_eq!(crtc, "eDP-1:80");
        assert!(queued_ago >= config.flip_overdue);
        flip.completed();
        let Report::FlipCompletedLate { crtc, after } = next(&reports) else {
            panic!("its completion is reported");
        };
        assert_eq!(crtc, "eDP-1:80");
        assert!(after >= queued_ago);

        drop(flip);
        drop(watchdog);
    }
}
