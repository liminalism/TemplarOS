//! `templar doctor freeze`: what a past boot left behind about a desktop freeze.
//!
//! Run after a hard restart. The compositor cannot report its own hang, so this reads everything
//! else that could have: the journal of the frozen boot (the compositor's watchdog lines, the
//! other daemons that kept logging, the user manager stopping the units, the kernel when this
//! user may read it), coredumps, pstore, and which of those channels are armed at all. It writes
//! one report directory and prints the triage answer.
//!
//! Strictly read-only on the system and never needs root, but reads more with it: `sudo templar
//! doctor freeze` writes the report under the invoking user's home and hands every file to them.
//! A missing tool or an unreadable file is a recorded gap, never a failure; the command fails only
//! when it cannot write the report.
//!
//! The analysis is pure functions over parsed [`Entry`] values, so every decision it makes is
//! tested against synthetic journals.

use crate::dev::cmd::CommandRunner;
use crate::humanize;
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The compositor watchdog's tracing target; every line it logs carries it.
pub const WATCHDOG_TARGET: &str = "templar_comp::watchdog";
/// The line a watchdog-equipped compositor logs once at startup: present means "armed", not an
/// incident.
const WATCHDOG_ARMED: &str = "compositor watchdog armed";
/// The compositor's idle-blanking lines, matched on message text rather than target. The last two
/// are what builds before the watchdog logged, and those are the builds the past boots ran.
const IDLE_MARKERS: &[&str] = &[
    "outputs blanked",
    "outputs woken",
    "idle timeout reached; turning the outputs off",
    "input after an idle blank",
];
/// templar-resourced's once-a-second heartbeat.
const TICK_MARKER: &str = "templar_resourced::daemon: tick";
/// "The last minutes" of a boot: the window `journal-last-15min.txt` and the tick analysis cover.
const LAST_WINDOW_US: u64 = 15 * 60 * 1_000_000;
/// How long after the compositor's last line something else must still be logging before the
/// machine as a whole counts as alive.
const ALIVE_AFTER_US: u64 = 60 * 1_000_000;
/// A templar unit taking longer than this to stop is flagged; healthy stops take ~0.2 s.
const SLOW_STOP_MS: u64 = 1_000;
/// Evidence lists are capped in the summary; the files and `--json` carry everything.
const SHOWN: usize = 40;
const MESSAGE_CHARS: usize = 300;
/// The root-only companion script that collects the system journal and arms the machine.
const SETUP_SCRIPT: &str = "scripts/freeze-evidence-setup.sh";
const PSTORE_ROOTS: &[&str] = &["/sys/fs/pstore", "/var/lib/systemd/pstore"];

/// One journal entry, reduced to what the analysis reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub realtime_us: u64,
    /// `SYSLOG_IDENTIFIER`, falling back to `_COMM`.
    pub identifier: String,
    pub pid: Option<u32>,
    pub user_unit: Option<String>,
    pub system_unit: Option<String>,
    /// `_TRANSPORT=kernel`.
    pub kernel: bool,
    pub priority: Option<u8>,
    pub message: String,
}

impl Entry {
    fn is_compositor(&self) -> bool {
        self.identifier == "templar-comp"
            || self.user_unit.as_deref() == Some("templar-comp.service")
    }

    /// systemd itself: PID 1 or a user manager.
    fn is_service_manager(&self) -> bool {
        self.identifier == "systemd"
    }

    fn is_user_manager(&self) -> bool {
        self.is_service_manager()
            && self
                .system_unit
                .as_deref()
                .is_some_and(|unit| unit.starts_with("user@"))
    }

    fn is_tick(&self) -> bool {
        self.message.contains(TICK_MARKER)
    }

    fn source(&self) -> &str {
        if self.kernel {
            "kernel"
        } else {
            &self.identifier
        }
    }
}

/// Parse `journalctl -o json` output (one object per line), oldest first. Lines that are not
/// journal objects are skipped.
pub fn parse_journal_json(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = text.lines().filter_map(parse_entry).collect();
    entries.sort_by_key(|entry| entry.realtime_us);
    entries
}

fn parse_entry(line: &str) -> Option<Entry> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let field = |name: &str| value.get(name).and_then(field_text);
    Some(Entry {
        realtime_us: field("__REALTIME_TIMESTAMP")?.parse().ok()?,
        identifier: field("SYSLOG_IDENTIFIER")
            .or_else(|| field("_COMM"))
            .unwrap_or_default(),
        pid: field("_PID").and_then(|pid| pid.parse().ok()),
        user_unit: field("_SYSTEMD_USER_UNIT"),
        system_unit: field("_SYSTEMD_UNIT"),
        kernel: field("_TRANSPORT").as_deref() == Some("kernel"),
        priority: field("PRIORITY").and_then(|priority| priority.parse().ok()),
        message: field("MESSAGE")
            .map(|message| strip_ansi(&message))
            .unwrap_or_default(),
    })
}

/// Journal fields are strings, except values that are not valid UTF-8 (or carry control bytes),
/// which `-o json` renders as an array of bytes.
fn field_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(items) => {
            let bytes: Vec<u8> = items
                .iter()
                .filter_map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                .collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

/// Remove terminal colour sequences (`ESC [ … final-byte`). The daemons' tracing output is
/// coloured even into the journal, and the colour codes split the very text the analysis matches:
/// `templar_resourced::daemon` `ESC[0m ESC[2m:` `ESC[0m tick`.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// A journal line quoted as evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evidence {
    pub at_us: u64,
    pub at: String,
    pub source: String,
    pub message: String,
}

impl Evidence {
    fn of(entry: &Entry) -> Self {
        Self {
            at_us: entry.realtime_us,
            at: at(entry.realtime_us),
            source: entry.source().to_owned(),
            message: entry.message.clone(),
        }
    }

    fn line(&self) -> String {
        format!("`{}` {}: {}", self.at, self.source, clip(&self.message))
    }
}

/// A realtime timestamp as `YYYY-MM-DD HH:MM:SS.mmm UTC`.
fn at(us: u64) -> String {
    let ms = (us / 1000) as i64;
    let stamp = humanize::timestamp_ms(ms);
    format!("{}.{:03} UTC", stamp.trim_end_matches(" UTC"), ms % 1000)
}

fn span(us: u64) -> String {
    if us < 1_000_000 {
        format!("{} ms", us / 1000)
    } else {
        humanize::duration_secs(us / 1_000_000)
    }
}

fn clip(message: &str) -> String {
    let message = message.trim_end();
    if message.chars().count() > MESSAGE_CHARS {
        format!(
            "{}…",
            message.chars().take(MESSAGE_CHARS).collect::<String>()
        )
    } else {
        message.to_owned()
    }
}

/// How the boot ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndingKind {
    /// The system shut down or rebooted on request (power key, logind, systemd-shutdown).
    Orderly,
    /// The user manager stopped the session in order; the system side was not visible.
    SessionStopped,
    /// No shutdown lines at all: the journal simply stops.
    Abrupt,
    /// No entries to judge by.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ending {
    pub kind: EndingKind,
    pub evidence: Vec<Evidence>,
}

const SYSTEM_SHUTDOWN_MARKERS: &[&str] = &[
    "Power key pressed",
    "System is powering down",
    "System is rebooting",
    "System is halting",
    "Journal stopped",
];

/// Orderly versus abrupt: whether the shutdown machinery left lines at the end of the boot.
pub fn ending(entries: &[Entry]) -> Ending {
    if entries.is_empty() {
        return Ending {
            kind: EndingKind::Unknown,
            evidence: Vec::new(),
        };
    }
    let mut evidence = Vec::new();
    let mut system = false;
    let mut session = false;
    for entry in entries {
        let system_line = entry.identifier == "systemd-shutdown"
            || SYSTEM_SHUTDOWN_MARKERS
                .iter()
                .any(|marker| entry.message.contains(marker))
            || (entry.is_service_manager()
                && !entry.is_user_manager()
                && entry.message.starts_with("Reached target shutdown.target"));
        let session_line = entry.is_user_manager()
            && (entry
                .message
                .starts_with("Activating special unit exit.target")
                || entry.message.starts_with("Reached target exit.target"));
        if system_line {
            system = true;
        } else if session_line {
            session |= entry.message.starts_with("Reached target exit.target");
        } else {
            continue;
        }
        evidence.push(Evidence::of(entry));
    }
    let kind = if system {
        EndingKind::Orderly
    } else if session {
        EndingKind::SessionStopped
    } else {
        EndingKind::Abrupt
    };
    Ending { kind, evidence }
}

/// A templar unit being stopped by its service manager.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnitStop {
    pub unit: String,
    pub stopping_us: u64,
    pub stopped_us: Option<u64>,
    pub duration_ms: Option<u64>,
}

/// The `templar-*` unit a manager line like `Stopping templar-comp.service - …...` is about.
fn templar_unit_after<'a>(message: &'a str, verb: &str) -> Option<&'a str> {
    let unit = message
        .strip_prefix(verb)?
        .strip_prefix(' ')?
        .split_whitespace()
        .next()?
        .trim_end_matches('.');
    (unit.starts_with("templar-") && unit.contains('.')).then_some(unit)
}

/// Pair every `Stopping <templar unit>` with the next `Stopped` of the same unit.
pub fn stop_durations(entries: &[Entry]) -> Vec<UnitStop> {
    let mut stops: Vec<UnitStop> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.is_service_manager()) {
        if let Some(unit) = templar_unit_after(&entry.message, "Stopping") {
            stops.push(UnitStop {
                unit: unit.to_owned(),
                stopping_us: entry.realtime_us,
                stopped_us: None,
                duration_ms: None,
            });
        } else if let Some(unit) = templar_unit_after(&entry.message, "Stopped")
            && let Some(stop) = stops
                .iter_mut()
                .find(|stop| stop.unit == unit && stop.stopped_us.is_none())
        {
            stop.stopped_us = Some(entry.realtime_us);
            stop.duration_ms = Some(entry.realtime_us.saturating_sub(stop.stopping_us) / 1000);
        }
    }
    stops
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Compositor {
    pub lines: usize,
    pub last_line: Option<Evidence>,
    pub watchdog: Vec<Evidence>,
    pub idle: Vec<Evidence>,
    pub unit_stops: Vec<UnitStop>,
    /// The managers' `templar-….service: …` lines: CPU consumed, timeouts, kills, failures.
    pub unit_notes: Vec<Evidence>,
}

pub fn compositor(entries: &[Entry]) -> Compositor {
    let matching = |predicate: &dyn Fn(&Entry) -> bool| -> Vec<Evidence> {
        entries
            .iter()
            .filter(|entry| predicate(entry))
            .map(Evidence::of)
            .collect()
    };
    Compositor {
        lines: entries.iter().filter(|entry| entry.is_compositor()).count(),
        last_line: entries
            .iter()
            .rev()
            .find(|entry| entry.is_compositor())
            .map(Evidence::of),
        watchdog: matching(&|entry| entry.message.contains(WATCHDOG_TARGET)),
        idle: matching(&|entry| {
            IDLE_MARKERS
                .iter()
                .any(|marker| entry.message.contains(marker))
        }),
        unit_stops: stop_durations(entries),
        unit_notes: matching(&|entry| {
            entry.is_service_manager()
                && entry.message.starts_with("templar-")
                && entry.message.contains(".service: ")
        }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TickGap {
    pub from: Evidence,
    pub to: Evidence,
    pub gap_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Liveness {
    /// From the compositor's last line to the boot's last entry.
    pub compositor_quiet_us: Option<u64>,
    /// Entries from anything but the compositor after its last line.
    pub after_compositor: usize,
    pub after_sources: BTreeMap<String, usize>,
    pub last_after: Option<Evidence>,
    /// Something else still logged at least a minute after the compositor's last line.
    pub machine_kept_running: bool,
    pub ticks_last_15min: usize,
    pub largest_tick_gap: Option<TickGap>,
    pub levels_last_15min: BTreeMap<String, usize>,
}

/// The `level=` of a resourced tick line.
fn tick_level(message: &str) -> Option<&str> {
    let rest = &message[message.find(TICK_MARKER)? + TICK_MARKER.len()..];
    rest.split_whitespace()
        .find_map(|token| token.strip_prefix("level="))
        .map(|level| level.trim_matches('"'))
}

fn last_window(entries: &[Entry]) -> &[Entry] {
    let Some(last) = entries.last() else {
        return entries;
    };
    let start = last.realtime_us.saturating_sub(LAST_WINDOW_US);
    &entries[entries.partition_point(|entry| entry.realtime_us < start)..]
}

/// Did the rest of the machine keep going after the compositor went quiet?
pub fn liveness(entries: &[Entry]) -> Liveness {
    let end = entries.last().map_or(0, |entry| entry.realtime_us);
    let last_comp = entries.iter().rev().find(|entry| entry.is_compositor());
    let after: Vec<&Entry> = last_comp.map_or_else(Vec::new, |comp| {
        entries
            .iter()
            .filter(|entry| entry.realtime_us > comp.realtime_us && !entry.is_compositor())
            .collect()
    });
    let mut after_sources = BTreeMap::new();
    for entry in &after {
        *after_sources.entry(entry.source().to_owned()).or_insert(0) += 1;
    }
    let ticks: Vec<&Entry> = last_window(entries)
        .iter()
        .filter(|entry| entry.is_tick())
        .collect();
    let largest_tick_gap = ticks
        .windows(2)
        .max_by_key(|pair| pair[1].realtime_us - pair[0].realtime_us)
        .map(|pair| TickGap {
            from: Evidence::of(pair[0]),
            to: Evidence::of(pair[1]),
            gap_ms: (pair[1].realtime_us - pair[0].realtime_us) / 1000,
        });
    let mut levels_last_15min = BTreeMap::new();
    for tick in &ticks {
        let level = tick_level(&tick.message).unwrap_or("?").to_owned();
        *levels_last_15min.entry(level).or_insert(0) += 1;
    }
    Liveness {
        compositor_quiet_us: last_comp.map(|comp| end - comp.realtime_us),
        after_compositor: after.len(),
        after_sources,
        last_after: after.last().map(|entry| Evidence::of(entry)),
        machine_kept_running: last_comp
            .zip(after.last())
            .is_some_and(|(comp, last)| last.realtime_us >= comp.realtime_us + ALIVE_AFTER_US),
        ticks_last_15min: ticks.len(),
        largest_tick_gap,
        levels_last_15min,
    }
}

/// The class of a kernel line that bears on a freeze, if it is one.
pub fn classify_kernel(message: &str) -> Option<&'static str> {
    const PATTERNS: &[(&str, &str)] = &[
        ("GPU HANG", "gpu-hang"),
        ("flip_done timed out", "flip-timeout"),
        ("NVRM: Xid", "nvidia-xid"),
        ("blocked for more than", "hung-task"),
        ("soft lockup", "soft-lockup"),
        ("hard LOCKUP", "hard-lockup"),
        ("detected stalls", "rcu-stall"),
        ("rcu: INFO", "rcu-stall"),
        ("invoked oom-killer", "oom"),
        ("Out of memory", "oom"),
        ("oom-kill", "oom"),
        ("BUG:", "bug"),
        ("Oops", "oops"),
        ("Call Trace", "call-trace"),
        ("BTRFS error", "btrfs"),
        ("BTRFS warning", "btrfs"),
        ("sysrq:", "sysrq"),
        ("SysRq :", "sysrq"),
        ("*ERROR*", "drm-error"),
    ];
    if let Some((_, class)) = PATTERNS
        .iter()
        .find(|(pattern, _)| message.contains(pattern))
    {
        return Some(class);
    }
    let lower = message.to_ascii_lowercase();
    if !lower.contains("error") {
        None
    } else if message.contains("[drm]") {
        Some("drm-error")
    } else if message.contains("nvidia-modeset") {
        Some("nvidia-modeset")
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KernelMatch {
    pub class: &'static str,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kernel {
    /// Any kernel line of this boot was readable.
    pub readable: bool,
    pub lines: usize,
    pub last_line: Option<Evidence>,
    pub matches: Vec<KernelMatch>,
    pub oomd_kills: Vec<Evidence>,
    pub journald_suppressed: Vec<Evidence>,
}

pub fn kernel(entries: &[Entry]) -> Kernel {
    let kernel_lines: Vec<&Entry> = entries.iter().filter(|entry| entry.kernel).collect();
    let matching = |identifier: &str, needle: &str| -> Vec<Evidence> {
        entries
            .iter()
            .filter(|entry| entry.identifier == identifier && entry.message.contains(needle))
            .map(Evidence::of)
            .collect()
    };
    Kernel {
        readable: !kernel_lines.is_empty(),
        lines: kernel_lines.len(),
        last_line: kernel_lines.last().map(|entry| Evidence::of(entry)),
        matches: kernel_lines
            .iter()
            .filter_map(|entry| {
                classify_kernel(&entry.message).map(|class| KernelMatch {
                    class,
                    evidence: Evidence::of(entry),
                })
            })
            .collect(),
        oomd_kills: matching("systemd-oomd", "Killed"),
        journald_suppressed: matching("systemd-journald", "Suppressed"),
    }
}

/// The journal rendered for `journal-last-15min.txt`: every entry on its own line, except that
/// runs of resourced's once-a-second tick collapse into one summary line.
pub fn collapse_ticks(entries: &[Entry]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut run: Vec<&Entry> = Vec::new();
    for entry in entries {
        if entry.is_tick() {
            run.push(entry);
        } else {
            flush_ticks(&mut run, &mut lines);
            lines.push(render_entry(entry));
        }
    }
    flush_ticks(&mut run, &mut lines);
    lines
}

fn flush_ticks(run: &mut Vec<&Entry>, lines: &mut Vec<String>) {
    match run.as_slice() {
        [] => {}
        [single] => lines.push(render_entry(single)),
        [first, .., last] => {
            let mut levels: Vec<(&str, usize)> = Vec::new();
            for tick in run.iter() {
                let level = tick_level(&tick.message).unwrap_or("?");
                match levels.iter_mut().find(|(seen, _)| *seen == level) {
                    Some((_, count)) => *count += 1,
                    None => levels.push((level, 1)),
                }
            }
            let levels: Vec<String> = levels
                .iter()
                .map(|(level, count)| format!("{level} ({count})"))
                .collect();
            lines.push(format!(
                "[{} resourced tick lines from {} to {}, levels: {}]",
                run.len(),
                at(first.realtime_us),
                at(last.realtime_us),
                levels.join(", ")
            ));
        }
    }
    run.clear();
}

fn render_entry(entry: &Entry) -> String {
    let pid = entry.pid.map(|pid| format!("[{pid}]")).unwrap_or_default();
    format!(
        "{} {}{pid}: {}",
        at(entry.realtime_us),
        entry.source(),
        entry.message
    )
}

/// Whether this user can read the system journal (kernel, logind, system units).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SystemJournal {
    pub readable: bool,
    pub detail: String,
}

/// What `coredumpctl` could say about the boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Coredumps {
    NotInstalled { core_pattern: String },
    Listed { list: String },
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CoredumpFindings {
    pub source: Coredumps,
    pub dumps: Vec<String>,
    pub templar: Vec<String>,
}

fn coredump_findings(source: &Coredumps) -> CoredumpFindings {
    let dumps: Vec<String> = match source {
        Coredumps::Listed { list } => list
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with("TIME"))
            .map(str::to_owned)
            .collect(),
        Coredumps::NotInstalled { .. } | Coredumps::Failed { .. } => Vec::new(),
    };
    let templar = dumps
        .iter()
        .filter(|line| line.contains("templar-"))
        .cloned()
        .collect();
    CoredumpFindings {
        source: source.clone(),
        dumps,
        templar,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Pstore {
    pub copied: Vec<String>,
    pub unreadable: Vec<String>,
}

/// Everything the analysis needs besides the journal entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// The resolved boot ID, or the argument when it could not be resolved.
    pub boot: String,
    pub reader: String,
    /// Why the boot's journal could not be read at all.
    pub journal_error: Option<String>,
    pub system_journal: SystemJournal,
    pub coredumps: Coredumps,
    pub pstore: Pstore,
    /// `kernel.sysrq` as armed now.
    pub sysrq: Option<u32>,
}

/// Something the report could not see, and the exact step that closes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Gap {
    pub what: String,
    pub why: String,
    pub fix: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Analysis {
    pub boot: String,
    pub reader: String,
    pub entries: usize,
    pub journal_error: Option<String>,
    pub system_journal: SystemJournal,
    pub first: Option<Evidence>,
    pub last: Option<Evidence>,
    pub ending: Ending,
    pub compositor: Compositor,
    pub liveness: Liveness,
    pub kernel: Kernel,
    pub coredumps: CoredumpFindings,
    pub pstore: Pstore,
    pub verdict: Vec<String>,
    pub gaps: Vec<Gap>,
}

pub fn analyse(entries: &[Entry], inputs: &Inputs) -> Analysis {
    let mut analysis = Analysis {
        boot: inputs.boot.clone(),
        reader: inputs.reader.clone(),
        entries: entries.len(),
        journal_error: inputs.journal_error.clone(),
        system_journal: inputs.system_journal.clone(),
        first: entries.first().map(Evidence::of),
        last: entries.last().map(Evidence::of),
        ending: ending(entries),
        compositor: compositor(entries),
        liveness: liveness(entries),
        kernel: kernel(entries),
        coredumps: coredump_findings(&inputs.coredumps),
        pstore: inputs.pstore.clone(),
        verdict: Vec::new(),
        gaps: Vec::new(),
    };
    analysis.gaps = gaps(&analysis, inputs.sysrq);
    analysis.verdict = verdict(&analysis);
    analysis
}

fn gaps(analysis: &Analysis, sysrq: Option<u32>) -> Vec<Gap> {
    let rerun = format!("sudo templar doctor freeze --boot {}", analysis.boot);
    let mut gaps = Vec::new();
    if let Some(error) = &analysis.journal_error {
        gaps.push(Gap {
            what: format!("the journal of boot {}", analysis.boot),
            why: error.clone(),
            fix: "run on the machine that froze, with journalctl (systemd) installed; \
                  `journalctl --list-boots` lists the boots it kept"
                .to_owned(),
        });
    }
    if !analysis.system_journal.readable {
        gaps.push(Gap {
            what: "the system journal: kernel messages (GPU hangs, hung tasks, OOM), logind's \
                   power-key and shutdown lines, system units, systemd-oomd, journald rate limiting"
                .to_owned(),
            why: analysis.system_journal.detail.clone(),
            fix: format!(
                "`{rerun}` now; for future reports `sudo usermod -aG adm $USER` (then log out \
                 and in) — or `sudo {SETUP_SCRIPT}`, which does both and arms the machine"
            ),
        });
    } else if !analysis.kernel.readable && analysis.journal_error.is_none() {
        gaps.push(Gap {
            what: "kernel messages of this boot".to_owned(),
            why: "the system journal is readable but holds no kernel line for this boot (a \
                  volatile journal loses them at power-off)"
                .to_owned(),
            fix: format!("`sudo {SETUP_SCRIPT}` makes the journal persistent"),
        });
    }
    if analysis.compositor.lines > 0 && analysis.compositor.watchdog.is_empty() {
        gaps.push(Gap {
            what: "what the compositor's main thread was doing when it went quiet".to_owned(),
            why: format!(
                "no `{WATCHDOG_TARGET}` line in this boot: the running templar-comp predates the \
                 event-loop watchdog (a build with it logs `{WATCHDOG_ARMED}` at startup)"
            ),
            fix: "run a templar-comp build with the watchdog; on the next freeze its `compositor \
                  event loop stalled` line names the main thread's state, wchan and syscall"
                .to_owned(),
        });
    }
    match &analysis.coredumps.source {
        Coredumps::NotInstalled { core_pattern } => gaps.push(Gap {
            what: "a core dump of a hung or aborted templar-comp".to_owned(),
            why: format!(
                "systemd-coredump is not installed; kernel.core_pattern = `{core_pattern}` leaves \
                 at most an unindexed `core` file in the process's working directory, and only \
                 if its core-size limit allows one"
            ),
            fix: format!("`sudo apt install systemd-coredump` (`sudo {SETUP_SCRIPT}` does it)"),
        }),
        Coredumps::Failed { error } => gaps.push(Gap {
            what: "the coredump list of this boot".to_owned(),
            why: error.clone(),
            fix: format!("`{rerun}`"),
        }),
        Coredumps::Listed { .. } => {}
    }
    if !analysis.pstore.unreadable.is_empty() {
        gaps.push(Gap {
            what: "pstore records (the kernel's last words before a panic)".to_owned(),
            why: analysis.pstore.unreadable.join("; "),
            fix: format!("`{rerun}`"),
        });
    }
    if let Some(sysrq) = sysrq
        && sysrq != 1
        && sysrq & 8 == 0
    {
        gaps.push(Gap {
            what: "a kernel dump of blocked tasks during the freeze (Alt+SysRq+W)".to_owned(),
            why: format!("kernel.sysrq = {sysrq} does not enable process dumps (bit 8)"),
            fix: format!("`sudo {SETUP_SCRIPT}`"),
        });
    }
    gaps
}

fn verdict(analysis: &Analysis) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(error) = &analysis.journal_error {
        lines.push(format!(
            "The journal of this boot is not readable: {}.",
            error.trim_end_matches('.')
        ));
        return lines;
    }
    lines.push(ending_sentence(analysis));
    let compositor = &analysis.compositor;
    let liveness = &analysis.liveness;
    if let (Some(last), Some(quiet)) = (&compositor.last_line, liveness.compositor_quiet_us) {
        lines.push(format!(
            "templar-comp last logged at {}, {} before the boot's last entry.",
            last.at,
            span(quiet)
        ));
    } else {
        lines.push("templar-comp logged nothing in this boot.".to_owned());
    }
    if let Some(after) = &liveness.last_after {
        let gap = liveness
            .largest_tick_gap
            .as_ref()
            .map(|gap| {
                format!(
                    "; resourced's tick never paused longer than {}",
                    seconds(gap.gap_ms)
                )
            })
            .unwrap_or_default();
        lines.push(if liveness.machine_kept_running {
            format!(
                "The machine kept running after that: {} entries from other processes, the last \
                 at {}{gap} — only the compositor stopped.",
                liveness.after_compositor, after.at
            )
        } else {
            format!(
                "Nothing else logged a minute or more after the compositor's last line (last other \
                 entry at {}): the whole system may have stopped.",
                after.at
            )
        });
    }
    let incidents: Vec<&Evidence> = compositor
        .watchdog
        .iter()
        .filter(|line| !line.message.contains(WATCHDOG_ARMED))
        .collect();
    lines.push(match incidents.first() {
        None if compositor.watchdog.is_empty() => "No compositor watchdog lines.".to_owned(),
        None => "The compositor watchdog was armed and logged no stall and no overdue flip: \
                 the event loop kept turning."
            .to_owned(),
        Some(first) => format!(
            "{} compositor watchdog report(s), the first: {}",
            incidents.len(),
            first.line()
        ),
    });
    if let Some(stop) = compositor
        .unit_stops
        .iter()
        .find(|stop| stop.unit == "templar-comp.service")
    {
        lines.push(match stop.duration_ms {
            Some(ms) if ms > SLOW_STOP_MS => {
                format!("templar-comp.service took {} to stop (slow).", seconds(ms))
            }
            Some(ms) => format!("templar-comp.service stopped in {}.", seconds(ms)),
            None => "templar-comp.service was told to stop and never reported stopped.".to_owned(),
        });
    }
    lines.push(kernel_sentence(analysis));
    lines
}

fn ending_sentence(analysis: &Analysis) -> String {
    let end = analysis
        .last
        .as_ref()
        .map_or_else(String::new, |last| format!(" at {}", last.at));
    match analysis.ending.kind {
        EndingKind::Orderly => format!("The boot ended in an orderly shutdown{end}."),
        EndingKind::SessionStopped if analysis.system_journal.readable => format!(
            "The user session was stopped in order{end} (user manager reached exit.target), \
             with no system shutdown lines."
        ),
        EndingKind::SessionStopped => format!(
            "The user session was stopped in order{end} (user manager reached exit.target) — a \
             requested shutdown, not a power cut; the system side is not readable."
        ),
        EndingKind::Abrupt => format!(
            "The boot ended abruptly{end}: no shutdown lines, the journal simply stops (hard \
             power-off or a hang)."
        ),
        EndingKind::Unknown => "No journal entries for this boot.".to_owned(),
    }
}

fn kernel_sentence(analysis: &Analysis) -> String {
    let kernel = &analysis.kernel;
    if !kernel.readable {
        return "Kernel messages: not readable.".to_owned();
    }
    if kernel.matches.is_empty() {
        return format!(
            "Kernel: {} lines, none matching a GPU/driver/hang/memory pattern.",
            kernel.lines
        );
    }
    format!(
        "Kernel: {} matching line(s) — {}.",
        kernel.matches.len(),
        class_counts(&kernel.matches)
    )
}

fn class_counts(matches: &[KernelMatch]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for found in matches {
        *counts.entry(found.class).or_insert(0) += 1;
    }
    counts
        .iter()
        .map(|(class, count)| format!("{class} ({count})"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03} s", ms / 1000, ms % 1000)
}

/// `summary.md`: the triage answer.
pub fn render_summary(analysis: &Analysis, dir: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Freeze report: boot {}\n", analysis.boot);
    let _ = writeln!(out, "- Report directory: {}", dir.display());
    let _ = writeln!(out, "- Read as: {}", analysis.reader);
    let _ = writeln!(
        out,
        "- System journal: {} — {}",
        if analysis.system_journal.readable {
            "readable"
        } else {
            "not readable"
        },
        analysis.system_journal.detail
    );
    let _ = writeln!(
        out,
        "- Journal entries read for this boot: {}",
        analysis.entries
    );
    let _ = writeln!(out, "\n## Verdict\n");
    for line in &analysis.verdict {
        let _ = writeln!(out, "- {line}");
    }
    render_ending(&mut out, analysis);
    render_compositor(&mut out, &analysis.compositor);
    render_liveness(&mut out, analysis);
    render_kernel(&mut out, analysis);
    render_dumps(&mut out, analysis);
    let _ = writeln!(out, "\n## 6. Gaps\n");
    if analysis.gaps.is_empty() {
        let _ = writeln!(out, "- None: every evidence channel was readable.");
    }
    for gap in &analysis.gaps {
        let _ = writeln!(
            out,
            "- **{}** — {}. Fix: {}",
            gap.what,
            gap.why.trim_end_matches('.'),
            gap.fix
        );
    }
    out
}

fn bullets(out: &mut String, indent: &str, evidence: &[Evidence]) {
    for item in evidence.iter().take(SHOWN) {
        let _ = writeln!(out, "{indent}- {}", item.line());
    }
    if evidence.len() > SHOWN {
        let _ = writeln!(
            out,
            "{indent}- … and {} more (see the journal files)",
            evidence.len() - SHOWN
        );
    }
}

fn not_readable(analysis: &Analysis) -> Option<String> {
    analysis
        .journal_error
        .as_ref()
        .map(|error| format!("- Not readable: {error}"))
}

fn render_ending(out: &mut String, analysis: &Analysis) {
    let _ = writeln!(out, "\n## 1. Boot window and ending\n");
    if let Some(line) = not_readable(analysis) {
        let _ = writeln!(out, "{line}");
        return;
    }
    if let (Some(first), Some(last)) = (&analysis.first, &analysis.last) {
        let _ = writeln!(out, "- First entry: {}", first.line());
        let _ = writeln!(out, "- Last entry: {}", last.line());
        let _ = writeln!(out, "- Boot lasted {}", span(last.at_us - first.at_us));
    }
    let _ = writeln!(out, "- Ending: {}", ending_sentence(analysis));
    if analysis.ending.evidence.is_empty() {
        let _ = writeln!(
            out,
            "  - no shutdown, power-key or exit.target line in the readable journal"
        );
    }
    bullets(out, "  ", &analysis.ending.evidence);
    if !analysis.system_journal.readable {
        let _ = writeln!(
            out,
            "  - logind (`Power key pressed`, `System is powering down`) and systemd-shutdown: not \
             readable — {}",
            analysis.system_journal.detail
        );
    }
}

fn render_compositor(out: &mut String, compositor: &Compositor) {
    let _ = writeln!(out, "\n## 2. Compositor\n");
    match &compositor.last_line {
        Some(last) => {
            let _ = writeln!(
                out,
                "- Last templar-comp line (of {} this boot): {}",
                compositor.lines,
                last.line()
            );
        }
        None => {
            let _ = writeln!(out, "- No templar-comp line in this boot.");
        }
    }
    let _ = writeln!(out, "- Watchdog (`{WATCHDOG_TARGET}`):");
    if compositor.watchdog.is_empty() {
        let _ = writeln!(out, "  - none in this boot");
    }
    bullets(out, "  ", &compositor.watchdog);
    let _ = writeln!(
        out,
        "- Idle blank/wake (`outputs blanked` / `outputs woken`):"
    );
    if compositor.idle.is_empty() {
        let _ = writeln!(out, "  - none in this boot");
    }
    bullets(out, "  ", &compositor.idle);
    let _ = writeln!(out, "- templar unit stops (`Stopping` → `Stopped`):");
    if compositor.unit_stops.is_empty() {
        let _ = writeln!(
            out,
            "  - no templar unit was stopped in this boot's readable journal"
        );
    }
    for stop in &compositor.unit_stops {
        let _ = match (stop.stopped_us, stop.duration_ms) {
            (Some(stopped), Some(ms)) => writeln!(
                out,
                "  - `{}`: `{}` → `{}` = **{}**{}",
                stop.unit,
                at(stop.stopping_us),
                at(stopped),
                seconds(ms),
                if ms > SLOW_STOP_MS {
                    " (slow: over 1 s)"
                } else {
                    ""
                }
            ),
            _ => writeln!(
                out,
                "  - `{}`: `{}` → never reported stopped",
                stop.unit,
                at(stop.stopping_us)
            ),
        };
    }
    if !compositor.unit_notes.is_empty() {
        let _ = writeln!(out, "- Service-manager notes on templar units:");
        bullets(out, "  ", &compositor.unit_notes);
    }
}

fn render_liveness(out: &mut String, analysis: &Analysis) {
    let _ = writeln!(out, "\n## 3. Liveness\n");
    if let Some(line) = not_readable(analysis) {
        let _ = writeln!(out, "{line}");
        return;
    }
    let liveness = &analysis.liveness;
    match (&analysis.compositor.last_line, &liveness.last_after) {
        (None, _) => {
            let _ = writeln!(out, "- No compositor line to measure from.");
        }
        (Some(_), None) => {
            let _ = writeln!(
                out,
                "- Nothing else logged after the compositor's last line."
            );
        }
        (Some(comp), Some(last)) => {
            let sources: Vec<String> = liveness
                .after_sources
                .iter()
                .map(|(source, count)| format!("{source} {count}"))
                .collect();
            let _ = writeln!(
                out,
                "- After the compositor's last line ({}): {} entries from other processes ({}), \
                 the last: {}",
                comp.at,
                liveness.after_compositor,
                sources.join(", "),
                last.line()
            );
            let _ = writeln!(
                out,
                "- {}",
                if liveness.machine_kept_running {
                    format!(
                        "**The machine kept running after the compositor went quiet**, for {}.",
                        span(last.at_us - comp.at_us)
                    )
                } else {
                    "Nothing else logged a minute or more after the compositor: the whole system \
                     may have stopped with it."
                        .to_owned()
                }
            );
        }
    }
    let _ = writeln!(
        out,
        "- templar-resourced ticks in the last 15 min of the boot: {}",
        liveness.ticks_last_15min
    );
    if let Some(gap) = &liveness.largest_tick_gap {
        let _ = writeln!(
            out,
            "  - largest gap between ticks: {} (`{}` → `{}`)",
            seconds(gap.gap_ms),
            gap.from.at,
            gap.to.at
        );
    }
    if !liveness.levels_last_15min.is_empty() {
        let levels: Vec<String> = liveness
            .levels_last_15min
            .iter()
            .map(|(level, count)| format!("{level} ({count})"))
            .collect();
        let _ = writeln!(out, "  - pressure levels seen: {}", levels.join(", "));
    }
}

fn render_kernel(out: &mut String, analysis: &Analysis) {
    let _ = writeln!(out, "\n## 4. Kernel\n");
    let kernel = &analysis.kernel;
    if kernel.readable {
        let _ = writeln!(out, "- {} kernel lines", kernel.lines);
        if let Some(last) = &kernel.last_line {
            let _ = writeln!(out, "- Last kernel line: {}", last.line());
        }
        if kernel.matches.is_empty() {
            let _ = writeln!(
                out,
                "- No line matches a GPU/driver/hang/memory/filesystem/sysrq pattern."
            );
        } else {
            let _ = writeln!(out, "- Matches by class: {}", class_counts(&kernel.matches));
            for found in kernel.matches.iter().take(SHOWN) {
                let _ = writeln!(out, "  - [{}] {}", found.class, found.evidence.line());
            }
            if kernel.matches.len() > SHOWN {
                let _ = writeln!(
                    out,
                    "  - … and {} more (journal-kernel.txt)",
                    kernel.matches.len() - SHOWN
                );
            }
        }
    } else {
        let reason = analysis.journal_error.as_ref().map_or_else(
            || {
                if analysis.system_journal.readable {
                    "the system journal holds no kernel line for this boot".to_owned()
                } else {
                    analysis.system_journal.detail.clone()
                }
            },
            Clone::clone,
        );
        let _ = writeln!(out, "- Not readable: {reason}");
    }
    for (title, evidence) in [
        ("systemd-oomd kills", &kernel.oomd_kills),
        (
            "journald rate limiting (`Suppressed`)",
            &kernel.journald_suppressed,
        ),
    ] {
        if !analysis.system_journal.readable {
            let _ = writeln!(out, "- {title}: not readable (system journal)");
        } else if evidence.is_empty() {
            let _ = writeln!(out, "- {title}: none");
        } else {
            let _ = writeln!(out, "- {title}:");
            bullets(out, "  ", evidence);
        }
    }
}

fn render_dumps(out: &mut String, analysis: &Analysis) {
    let _ = writeln!(out, "\n## 5. Coredumps and pstore\n");
    let dumps = &analysis.coredumps;
    let _ = match &dumps.source {
        Coredumps::NotInstalled { core_pattern } => writeln!(
            out,
            "- Coredumps: not readable — coredumpctl is not installed; kernel.core_pattern = \
             `{core_pattern}`"
        ),
        Coredumps::Failed { error } => writeln!(out, "- Coredumps: not readable — {error}"),
        Coredumps::Listed { .. } if dumps.dumps.is_empty() => {
            writeln!(out, "- Coredumps: none in this boot's window")
        }
        Coredumps::Listed { .. } => writeln!(
            out,
            "- Coredumps: {} in this boot's window, {} of templar executables (coredumps.txt)",
            dumps.dumps.len(),
            dumps.templar.len()
        ),
    };
    for line in &dumps.templar {
        let _ = writeln!(out, "  - `{}`", line.trim());
    }
    let pstore = &analysis.pstore;
    if pstore.copied.is_empty() {
        let _ = writeln!(out, "- pstore: no readable record");
    } else {
        let _ = writeln!(
            out,
            "- pstore: {} record(s) copied to pstore/",
            pstore.copied.len()
        );
        for record in &pstore.copied {
            let _ = writeln!(out, "  - {record}");
        }
    }
    for unreadable in &pstore.unreadable {
        let _ = writeln!(out, "  - not readable: {unreadable}");
    }
}

/// The boot to examine: its ID when known, and how to name it to journalctl and the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    pub id: Option<String>,
    pub requested: String,
}

impl Boot {
    /// What `journalctl -b` gets: the stable ID when it was resolved.
    pub fn arg(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.requested)
    }

    /// The report directory's suffix.
    pub fn label(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("offset{}", self.requested.replace('/', "_")))
    }
}

fn is_boot_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Resolve `--boot` (an offset like `-1`, or a boot ID) against `journalctl --list-boots`.
pub fn resolve_boot(requested: &str, list_boots: &str) -> Boot {
    let id = if is_boot_id(requested) {
        Some(requested.to_ascii_lowercase())
    } else {
        requested.parse::<i64>().ok().and_then(|offset| {
            list_boots.lines().find_map(|line| {
                let mut tokens = line.split_whitespace();
                let index = tokens.next()?.parse::<i64>().ok()?;
                let id = tokens.next()?;
                (index == offset && is_boot_id(id)).then(|| id.to_owned())
            })
        })
    };
    Boot {
        id,
        requested: requested.to_owned(),
    }
}

/// `templar doctor freeze`'s arguments.
#[derive(Clone, Debug)]
pub struct Options {
    /// A journal boot offset (`-1`) or boot ID.
    pub boot: String,
    /// Write `boot-<id>/` under this directory instead of the state directory.
    pub out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub dir: PathBuf,
    #[serde(skip)]
    pub summary: String,
    pub analysis: Analysis,
}

/// Collect the evidence for one boot, write the report directory and analyse it.
///
/// # Errors
/// Only when the report directory cannot be written; everything unreadable is a gap.
pub fn collect(options: &Options, runner: &dyn CommandRunner) -> Result<Report> {
    let identity = Identity::current();
    let boots = run_tool(runner, "journalctl", &["--list-boots", "--no-pager"]);
    let boot = resolve_boot(&options.boot, &boots.stdout);
    let base = options
        .out
        .clone()
        .unwrap_or_else(|| identity.reports_root.clone());
    let dir = base.join(format!("boot-{}", boot.label()));
    let writer = Writer::prepare(&dir, identity.owner)?;
    writer.file("boots.txt", boots.text().as_bytes())?;

    let json = run_tool(
        runner,
        "journalctl",
        &["-b", boot.arg(), "-o", "json", "--no-pager"],
    );
    let entries = parse_journal_json(&json.stdout);
    let journal_error = (entries.is_empty()).then(|| json.why_empty("journalctl"));
    for (name, kernel) in [("journal-all.txt", false), ("journal-kernel.txt", true)] {
        let mut args = vec!["-b", boot.arg()];
        if kernel {
            args.push("-k");
        }
        args.extend(["-o", "short-precise", "--no-pager"]);
        writer.file(
            name,
            run_tool(runner, "journalctl", &args).text().as_bytes(),
        )?;
    }
    let mut last = collapse_ticks(last_window(&entries)).join("\n");
    last.push('\n');
    writer.file("journal-last-15min.txt", last.as_bytes())?;

    let window = entries.first().zip(entries.last()).map(|(first, last)| {
        (
            first.realtime_us / 1_000_000,
            last.realtime_us / 1_000_000 + 1,
        )
    });
    let (coredumps, coredump_text) = collect_coredumps(runner, window);
    writer.file("coredumps.txt", coredump_text.as_bytes())?;
    let pstore = copy_pstore(&writer)?;
    let system_journal = system_journal_readability(identity.euid);
    writer.file(
        "environment.txt",
        environment_text(runner, &system_journal).as_bytes(),
    )?;

    let inputs = Inputs {
        boot: boot.arg().to_owned(),
        reader: identity.reader.clone(),
        journal_error,
        system_journal,
        coredumps,
        pstore,
        sysrq: read_trimmed("/proc/sys/kernel/sysrq")
            .ok()
            .and_then(|value| value.parse().ok()),
    };
    let analysis = analyse(&entries, &inputs);
    let summary = render_summary(&analysis, &dir);
    writer.file("summary.md", summary.as_bytes())?;
    Ok(Report {
        dir,
        summary,
        analysis,
    })
}

/// Who is running this, and therefore where the report goes and who owns it.
struct Identity {
    euid: u32,
    reader: String,
    reports_root: PathBuf,
    /// Under `sudo`: the invoking user, who is handed every file written.
    owner: Option<(u32, u32)>,
}

impl Identity {
    fn current() -> Self {
        let euid = read_trimmed("/proc/self/status")
            .ok()
            .and_then(|status| effective_uid(&status))
            .unwrap_or(u32::MAX);
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        let sudo = || {
            let user = std::env::var("SUDO_USER").ok()?;
            let uid = std::env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
            let gid = std::env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
            Some((user, uid, gid))
        };
        if euid == 0
            && let Some((user, uid, gid)) = sudo()
        {
            let home =
                passwd_home(&passwd, &user).unwrap_or_else(|| PathBuf::from("/home").join(&user));
            return Self {
                euid,
                reader: format!("root (via sudo from {user}, uid {uid})"),
                reports_root: home.join(".local/state/templaros/freeze-reports"),
                owner: Some((uid, gid)),
            };
        }
        let name = passwd_name(&passwd, euid).unwrap_or_else(|| "?".to_owned());
        Self {
            euid,
            reader: format!("{name} (uid {euid})"),
            reports_root: templar_state::default_path()
                .parent()
                .map_or_else(|| PathBuf::from("/var/tmp/templaros"), Path::to_path_buf)
                .join("freeze-reports"),
            owner: None,
        }
    }
}

/// The effective UID from `/proc/self/status` (`Uid: real effective saved fs`).
fn effective_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn passwd_home(passwd: &str, user: &str) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 6 && fields[0] == user).then(|| PathBuf::from(fields[5]))
    })
}

fn passwd_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 3 && fields[2].parse() == Ok(uid)).then(|| fields[0].to_owned())
    })
}

/// Writes the report directory, replacing an earlier run's, and hands everything to `owner`.
struct Writer {
    dir: PathBuf,
    owner: Option<(u32, u32)>,
}

impl Writer {
    fn prepare(dir: &Path, owner: Option<(u32, u32)>) -> Result<Self> {
        let missing: Vec<PathBuf> = dir
            .ancestors()
            .skip(1)
            .take_while(|ancestor| !ancestor.exists())
            .map(Path::to_path_buf)
            .collect();
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("removing the earlier report {}", dir.display()))?;
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let writer = Self {
            dir: dir.to_path_buf(),
            owner,
        };
        for created in missing.iter().rev() {
            writer.hand_over(created)?;
        }
        writer.hand_over(dir)?;
        Ok(writer)
    }

    fn hand_over(&self, path: &Path) -> Result<()> {
        if let Some((uid, gid)) = self.owner {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))
                .with_context(|| format!("handing {} to uid {uid}", path.display()))?;
        }
        Ok(())
    }

    /// Write `name` (relative to the report directory, subdirectories created as needed).
    fn file(&self, name: &str, contents: &[u8]) -> Result<()> {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            self.hand_over(parent)?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        self.hand_over(&path)
    }
}

/// One external command's result, kept whether or not it worked.
struct ToolRun {
    command: String,
    stdout: String,
    stderr: String,
    /// How it failed: did not start, timed out, or exited non-zero.
    failure: Option<String>,
    /// The program is not installed.
    missing: bool,
}

fn run_tool(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> ToolRun {
    let command = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match runner.run(Path::new("/"), program, args) {
        Ok(output) => ToolRun {
            command,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            failure: (!output.status.success()).then(|| format!("exited with {}", output.status)),
            missing: false,
        },
        Err(error) => ToolRun {
            command,
            stdout: String::new(),
            stderr: String::new(),
            missing: error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }),
            failure: Some(format!("{error:#}")),
        },
    }
}

impl ToolRun {
    /// The report file for this command: what it printed, then its stderr and failure as `#`
    /// lines so the file says why it is short.
    fn text(&self) -> String {
        let mut text = format!("# $ {}\n", self.command);
        text.push_str(&self.stdout);
        if !self.stdout.is_empty() && !self.stdout.ends_with('\n') {
            text.push('\n');
        }
        for line in self.stderr.lines() {
            let _ = writeln!(text, "# stderr: {line}");
        }
        if self.missing {
            let program = self.command.split(' ').next().unwrap_or_default();
            let _ = writeln!(text, "# {program} is not installed");
        } else if let Some(failure) = &self.failure {
            let _ = writeln!(text, "# failed: {failure}");
        }
        if self.stdout.is_empty() && self.stderr.is_empty() && self.failure.is_none() {
            text.push_str("# (no output: nothing readable)\n");
        }
        text
    }

    fn why_empty(&self, program: &str) -> String {
        if self.missing {
            return format!("{program} is not installed");
        }
        let stderr: Vec<&str> = self
            .stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        match (&self.failure, stderr.is_empty()) {
            (Some(failure), true) => format!("`{}` {failure}", self.command),
            (Some(failure), false) => format!("`{}` {failure}: {}", self.command, stderr.join(" ")),
            (None, _) => format!("`{}` returned no entry this user can read", self.command),
        }
    }
}

fn collect_coredumps(
    runner: &dyn CommandRunner,
    window: Option<(u64, u64)>,
) -> (Coredumps, String) {
    let mut range = Vec::new();
    if let Some((since, until)) = window {
        range = vec![
            "--since".to_owned(),
            format!("@{since}"),
            "--until".to_owned(),
            format!("@{until}"),
        ];
    }
    let mut args: Vec<&str> = vec!["list", "--no-pager"];
    args.extend(range.iter().map(String::as_str));
    let list = run_tool(runner, "coredumpctl", &args);
    if list.missing {
        let core_pattern = read_trimmed("/proc/sys/kernel/core_pattern")
            .unwrap_or_else(|error| format!("(unreadable: {error})"));
        let text = format!(
            "coredumpctl is not installed (no systemd-coredump)\n\
             kernel.core_pattern = {core_pattern}\n"
        );
        return (Coredumps::NotInstalled { core_pattern }, text);
    }
    let mut text = list.text();
    if list.failure.is_some() && !list.stderr.contains("No coredumps found") {
        return (
            Coredumps::Failed {
                error: list.why_empty("coredumpctl"),
            },
            text,
        );
    }
    let mut executables: Vec<&str> = list
        .stdout
        .split_whitespace()
        .filter(|token| token.starts_with('/') && token.contains("/templar-"))
        .collect();
    executables.dedup();
    for executable in executables {
        let mut info: Vec<&str> = vec!["info", "--no-pager"];
        info.extend(range.iter().map(String::as_str));
        info.push(executable);
        text.push('\n');
        text.push_str(&run_tool(runner, "coredumpctl", &info).text());
    }
    (
        Coredumps::Listed {
            list: list.stdout.clone(),
        },
        text,
    )
}

/// Copy every readable pstore record into `pstore/`, noting what could not be read.
fn copy_pstore(writer: &Writer) -> Result<Pstore> {
    let mut pstore = Pstore::default();
    let mut pending: Vec<PathBuf> = PSTORE_ROOTS.iter().map(PathBuf::from).collect();
    while let Some(path) = pending.pop() {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                pstore
                    .unreadable
                    .push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        if metadata.is_dir() {
            match std::fs::read_dir(&path) {
                Ok(children) => pending.extend(children.flatten().map(|child| child.path())),
                Err(error) => pstore
                    .unreadable
                    .push(format!("{}: {error}", path.display())),
            }
        } else if metadata.is_file() {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let flat = path
                        .to_string_lossy()
                        .trim_start_matches('/')
                        .replace('/', "__");
                    writer.file(&format!("pstore/{flat}"), &bytes)?;
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or_else(|| "?".to_owned(), |since| at(since.as_micros() as u64));
                    pstore.copied.push(format!(
                        "{} ({} bytes, modified {modified})",
                        path.display(),
                        bytes.len()
                    ));
                }
                Err(error) => pstore
                    .unreadable
                    .push(format!("{}: {error}", path.display())),
            }
        }
    }
    pstore.copied.sort();
    let mut index = String::from("# pstore records copied here\n");
    for line in &pstore.copied {
        let _ = writeln!(index, "{line}");
    }
    index.push_str("# not readable\n");
    for line in &pstore.unreadable {
        let _ = writeln!(index, "{line}");
    }
    writer.file("pstore/INDEX.txt", index.as_bytes())?;
    Ok(pstore)
}

fn system_journal_readability(euid: u32) -> SystemJournal {
    let mut denied = Vec::new();
    for root in ["/var/log/journal", "/run/log/journal"] {
        let Ok(machines) = std::fs::read_dir(root) else {
            continue;
        };
        for machine in machines.flatten() {
            let Ok(files) = std::fs::read_dir(machine.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if !(name.starts_with("system") && name.ends_with(".journal")) {
                    continue;
                }
                match std::fs::File::open(file.path()) {
                    Ok(_) => {
                        return SystemJournal {
                            readable: true,
                            detail: format!("{} opens", file.path().display()),
                        };
                    }
                    Err(error) => denied.push(format!("{}: {error}", file.path().display())),
                }
            }
        }
    }
    let detail = match denied.first() {
        None => "no system journal file under /var/log/journal or /run/log/journal".to_owned(),
        Some(first) if euid != 0 => {
            format!("{first} (only root and the adm and systemd-journal groups may read it)")
        }
        Some(first) => first.clone(),
    };
    SystemJournal {
        readable: false,
        detail,
    }
}

fn read_trimmed(path: &str) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(|text| text.trim().to_owned())
}

/// `environment.txt`: the evidence channels armed right now, for the next freeze.
fn environment_text(runner: &dyn CommandRunner, system_journal: &SystemJournal) -> String {
    let mut text = String::from("# Evidence channels armed now\n");
    for (name, path) in [
        ("kernel.sysrq", "/proc/sys/kernel/sysrq"),
        (
            "kernel.hung_task_timeout_secs",
            "/proc/sys/kernel/hung_task_timeout_secs",
        ),
        (
            "kernel.hung_task_warnings",
            "/proc/sys/kernel/hung_task_warnings",
        ),
        (
            "kernel.softlockup_panic",
            "/proc/sys/kernel/softlockup_panic",
        ),
        (
            "kernel.hardlockup_panic",
            "/proc/sys/kernel/hardlockup_panic",
        ),
        ("kernel.panic", "/proc/sys/kernel/panic"),
        ("kernel.core_pattern", "/proc/sys/kernel/core_pattern"),
        ("pstore backend", "/sys/module/pstore/parameters/backend"),
    ] {
        let value = read_trimmed(path).unwrap_or_else(|error| format!("(unreadable: {error})"));
        let _ = writeln!(text, "{name} = {value}");
    }
    let id = run_tool(runner, "id", &[]);
    let _ = writeln!(
        text,
        "id: {}",
        id.failure.as_deref().unwrap_or_else(|| id.stdout.trim())
    );
    let _ = writeln!(
        text,
        "system journal readable: {} — {}",
        if system_journal.readable { "yes" } else { "no" },
        system_journal.detail
    );
    for limits in compositor_core_limits() {
        let _ = writeln!(text, "{limits}");
    }
    text.push_str("\n# journald configuration\n");
    for file in [
        "/etc/systemd/journald.conf",
        "/usr/lib/systemd/journald.conf",
    ] {
        if let Ok(contents) = std::fs::read_to_string(file) {
            let settings: Vec<&str> = contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect();
            let _ = writeln!(text, "{file}: {}", settings.join(" "));
        }
    }
    for dir in [
        "/etc/systemd/journald.conf.d",
        "/run/systemd/journald.conf.d",
        "/usr/lib/systemd/journald.conf.d",
    ] {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
            Err(error) => {
                let _ = writeln!(text, "{dir}: {error}");
                continue;
            }
        };
        files.sort();
        if files.is_empty() {
            let _ = writeln!(text, "{dir}: (empty)");
        }
        for file in files {
            let contents = std::fs::read_to_string(&file)
                .unwrap_or_else(|error| format!("(unreadable: {error})"));
            let _ = writeln!(text, "{}:", file.display());
            for line in contents.lines() {
                let _ = writeln!(text, "    {line}");
            }
        }
    }
    text
}

/// The core-size limit of every running templar-comp: with no systemd-coredump, a zero soft limit
/// means an abort leaves nothing at all.
fn compositor_core_limits() -> Vec<String> {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    processes
        .flatten()
        .filter(|process| {
            std::fs::read_to_string(process.path().join("comm"))
                .is_ok_and(|comm| comm.trim() == "templar-comp")
        })
        .filter_map(|process| {
            let limits = std::fs::read_to_string(process.path().join("limits")).ok()?;
            let line = limits
                .lines()
                .find(|line| line.starts_with("Max core file size"))?;
            Some(format!(
                "templar-comp pid {}: {}",
                process.file_name().to_string_lossy(),
                line.split_whitespace().collect::<Vec<_>>().join(" ")
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_790_818_000_000_000;
    const SECOND: u64 = 1_000_000;

    fn entry(at_s: u64, identifier: &str, message: &str) -> Entry {
        Entry {
            realtime_us: T0 + at_s * SECOND,
            identifier: identifier.to_owned(),
            pid: Some(100),
            user_unit: Some(format!("{identifier}.service")),
            system_unit: Some("user@1000.service".to_owned()),
            kernel: false,
            priority: Some(6),
            message: message.to_owned(),
        }
    }

    fn user_manager(at_s: u64, message: &str) -> Entry {
        Entry {
            user_unit: Some("init.scope".to_owned()),
            ..entry(at_s, "systemd", message)
        }
    }

    fn kernel_line(at_s: u64, message: &str) -> Entry {
        Entry {
            kernel: true,
            user_unit: None,
            system_unit: None,
            ..entry(at_s, "kernel", message)
        }
    }

    fn comp(at_s: u64, message: &str) -> Entry {
        entry(at_s, "templar-comp", message)
    }

    fn tick(at_us: u64, level: &str) -> Entry {
        Entry {
            realtime_us: T0 + at_us,
            ..entry(
                0,
                "templar-resourced",
                &format!(
                    "2026-10-01T01:40:29.309520Z  INFO templar_resourced::daemon: tick \
                     level={level} instances=1 actions=1 wakeup=\"fallback\""
                ),
            )
        }
    }

    fn inputs(system_readable: bool) -> Inputs {
        Inputs {
            boot: "a2228e0a709d40509c284f6698e0feb2".to_owned(),
            reader: "dk (uid 1000)".to_owned(),
            journal_error: None,
            system_journal: SystemJournal {
                readable: system_readable,
                detail: "Permission denied".to_owned(),
            },
            coredumps: Coredumps::Listed {
                list: String::new(),
            },
            pstore: Pstore::default(),
            sysrq: Some(1),
        }
    }

    #[test]
    fn a_power_key_shutdown_is_orderly_a_session_exit_alone_is_a_session_stop_silence_is_abrupt() {
        let session = vec![
            comp(0, "toplevel mapped"),
            user_manager(10, "Activating special unit exit.target..."),
            user_manager(12, "Reached target exit.target - Exit the Session."),
        ];
        let result = ending(&session);
        assert_eq!(result.kind, EndingKind::SessionStopped);
        assert_eq!(result.evidence.len(), 2);

        let mut system = session.clone();
        system.insert(1, entry(9, "systemd-logind", "Power key pressed short."));
        let result = ending(&system);
        assert_eq!(result.kind, EndingKind::Orderly);
        assert_eq!(result.evidence[0].source, "systemd-logind");

        // `exit.target` merely being activated is no proof the session got there.
        let interrupted = vec![
            comp(0, "toplevel mapped"),
            user_manager(10, "Activating special unit exit.target..."),
        ];
        assert_eq!(ending(&interrupted).kind, EndingKind::Abrupt);
        assert_eq!(
            ending(&[comp(0, "toplevel mapped")]).kind,
            EndingKind::Abrupt
        );
        assert_eq!(ending(&[]).kind, EndingKind::Unknown);
    }

    #[test]
    fn the_machine_kept_running_when_others_log_long_after_the_compositor_went_quiet() {
        let mut entries = vec![comp(0, "toplevel destroyed")];
        entries.extend((1..=811).map(|s| tick(s * SECOND, "normal")));
        entries.push(user_manager(
            812,
            "Stopping templar-comp.service - compositor...",
        ));
        let live = liveness(&entries);
        assert!(live.machine_kept_running);
        assert_eq!(live.after_compositor, 812);
        assert_eq!(live.after_sources["templar-resourced"], 811);
        assert_eq!(live.compositor_quiet_us, Some(812 * SECOND));
        assert_eq!(live.largest_tick_gap.as_ref().unwrap().gap_ms, 1000);
        assert_eq!(live.levels_last_15min["normal"], 811);

        // Everything stopping together is a different story: the whole machine went down.
        let together = vec![comp(0, "toplevel destroyed"), tick(30 * SECOND, "normal")];
        assert!(!liveness(&together).machine_kept_running);
    }

    #[test]
    fn the_largest_tick_gap_names_both_ends_and_only_the_last_15_minutes_count() {
        let entries = vec![
            tick(0, "normal"),
            tick(SECOND, "normal"),
            // Over 15 minutes before the end: outside the window.
            tick(1000 * SECOND, "normal"),
            tick(1001 * SECOND, "elevated"),
            tick(1008 * SECOND + 500_000, "elevated"),
            tick(1009 * SECOND, "normal"),
        ];
        let live = liveness(&entries);
        assert_eq!(live.ticks_last_15min, 4);
        let gap = live.largest_tick_gap.unwrap();
        assert_eq!(gap.gap_ms, 7500);
        assert_eq!(gap.from.at_us, T0 + 1001 * SECOND);
        assert_eq!(live.levels_last_15min["elevated"], 2);
        assert_eq!(live.levels_last_15min["normal"], 2);
    }

    #[test]
    fn watchdog_and_idle_lines_are_extracted_from_the_compositor_stream() {
        let entries = vec![
            comp(0, "INFO templar_comp::xdg_shell: toplevel mapped"),
            comp(
                5,
                "WARN templar_comp::watchdog: compositor event loop stalled stalled_ms=7004 \
                 main_state=D main_wchan=drm_atomic_helper_wait_for_flip_done",
            ),
            comp(
                6,
                "INFO templar_comp::backend: outputs blanked after idle timeout",
            ),
            comp(
                9,
                "INFO templar_comp::watchdog: compositor event loop recovered stalled_ms=9100",
            ),
            comp(
                10,
                "INFO templar_comp::backend: outputs woken from idle blank",
            ),
        ];
        let result = compositor(&entries);
        assert_eq!(result.lines, 5);
        let watchdog: Vec<u64> = result.watchdog.iter().map(|e| e.at_us).collect();
        assert_eq!(watchdog, vec![T0 + 5 * SECOND, T0 + 9 * SECOND]);
        assert_eq!(result.idle.len(), 2);
        assert_eq!(result.last_line.unwrap().at_us, T0 + 10 * SECOND);
    }

    #[test]
    fn stop_durations_pair_each_stopping_with_its_own_stopped() {
        let mut stopped_late =
            user_manager(0, "Stopped templar-comp.service - TemplarOS compositor.");
        stopped_late.realtime_us = T0 + 1_451_000;
        let entries = vec![
            user_manager(0, "Stopping templar-comp.service - TemplarOS compositor..."),
            user_manager(
                0,
                "Stopping templar-filed.service - TemplarOS file service...",
            ),
            user_manager(0, "Stopping pipewire.service - PipeWire..."),
            Entry {
                realtime_us: T0 + 16_000,
                ..user_manager(0, "Stopped templar-filed.service - TemplarOS file service.")
            },
            user_manager(0, "Stopping templar-shell.service - shell..."),
            stopped_late,
        ];
        let stops = stop_durations(&entries);
        let summary: Vec<(&str, Option<u64>)> = stops
            .iter()
            .map(|stop| (stop.unit.as_str(), stop.duration_ms))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("templar-comp.service", Some(1451)),
                ("templar-filed.service", Some(16)),
                ("templar-shell.service", None),
            ]
        );
    }

    #[test]
    fn kernel_lines_are_classified_by_what_they_say_about_a_hang() {
        let cases = [
            (
                "i915 0000:00:02.0: [drm] GPU HANG: ecode 12:1:85dffffb",
                Some("gpu-hang"),
            ),
            (
                "[drm:drm_atomic_helper_wait_for_flip_done] *ERROR* [CRTC:82:pipe A] flip_done timed out",
                Some("flip-timeout"),
            ),
            (
                "i915 0000:00:02.0: [drm] *ERROR* Atomic update failure on pipe A",
                Some("drm-error"),
            ),
            (
                "amdgpu 0000:03:00.0: [drm] Error in DP aux read",
                Some("drm-error"),
            ),
            (
                "NVRM: Xid (PCI:0000:01:00): 79, GPU has fallen off the bus.",
                Some("nvidia-xid"),
            ),
            (
                "nvidia-modeset: ERROR: GPU:0: Idling display engine timed out",
                Some("nvidia-modeset"),
            ),
            (
                "INFO: task kworker/u16:3:123 blocked for more than 120 seconds.",
                Some("hung-task"),
            ),
            (
                "watchdog: BUG: soft lockup - CPU#2 stuck for 22s! [templar-comp:1647]",
                Some("soft-lockup"),
            ),
            (
                "Watchdog detected hard LOCKUP on cpu 3",
                Some("hard-lockup"),
            ),
            (
                "rcu: INFO: rcu_preempt detected stalls on CPUs/tasks:",
                Some("rcu-stall"),
            ),
            (
                "templar-comp invoked oom-killer: gfp_mask=0x140cca",
                Some("oom"),
            ),
            (
                "Out of memory: Killed process 1647 (templar-comp)",
                Some("oom"),
            ),
            (
                "BUG: kernel NULL pointer dereference, address: 0000000000000008",
                Some("bug"),
            ),
            ("Call Trace:", Some("call-trace")),
            (
                "BTRFS error (device nvme0n1p2): bdev /dev/nvme0n1p2 errs: wr 0",
                Some("btrfs"),
            ),
            ("sysrq: Show Blocked State", Some("sysrq")),
            (
                "[drm] Initialized i915 1.6.0 for 0000:00:02.0 on minor 1",
                None,
            ),
            ("usb 1-2: new high-speed USB device number 3", None),
        ];
        for (message, expected) in cases {
            assert_eq!(classify_kernel(message), expected, "{message}");
        }
    }

    #[test]
    fn only_kernel_transport_lines_are_kernel_evidence() {
        let entries = vec![
            // A user process quoting a kernel phrase is not the kernel saying it.
            comp(0, "blocked for more than a second waiting for a flip"),
            kernel_line(
                1,
                "INFO: task templar-comp:1647 blocked for more than 120 seconds.",
            ),
            kernel_line(2, "usb 1-2: new device"),
            entry(
                3,
                "systemd-oomd",
                "Killed /user.slice/app.scope due to memory pressure",
            ),
            entry(
                4,
                "systemd-journald",
                "Suppressed 812 messages from templar-resourced.service",
            ),
        ];
        let result = kernel(&entries);
        assert!(result.readable);
        assert_eq!(result.lines, 2);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].class, "hung-task");
        assert_eq!(result.oomd_kills.len(), 1);
        assert_eq!(result.journald_suppressed.len(), 1);
        assert!(!kernel(&entries[..1]).readable);
    }

    #[test]
    fn consecutive_resourced_ticks_collapse_into_one_line_with_their_levels() {
        let entries = vec![
            tick(0, "normal"),
            tick(SECOND, "normal"),
            tick(2 * SECOND, "elevated"),
            comp(3, "toplevel mapped"),
            tick(4 * SECOND, "normal"),
        ];
        let lines = collapse_ticks(&entries);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            format!(
                "[3 resourced tick lines from {} to {}, levels: normal (2), elevated (1)]",
                at(T0),
                at(T0 + 2 * SECOND)
            )
        );
        assert!(lines[1].ends_with("templar-comp[100]: toplevel mapped"));
        // A lone tick is left as it was.
        assert!(lines[2].contains("templar_resourced::daemon: tick level=normal"));
    }

    #[test]
    fn an_unreadable_system_journal_is_a_gap_that_names_the_fix() {
        let entries = vec![comp(0, "toplevel mapped")];
        let analysis = analyse(&entries, &inputs(false));
        let gap = analysis
            .gaps
            .iter()
            .find(|gap| gap.what.starts_with("the system journal"))
            .expect("a system-journal gap");
        assert!(gap.fix.contains("sudo usermod -aG adm $USER"));
        assert!(
            gap.fix
                .contains("sudo templar doctor freeze --boot a2228e0a709d40509c284f6698e0feb2")
        );
        assert!(gap.fix.contains(SETUP_SCRIPT));

        let readable = analyse(&entries, &inputs(true));
        assert!(
            !readable
                .gaps
                .iter()
                .any(|gap| gap.what.starts_with("the system journal"))
        );
    }

    #[test]
    fn a_missing_watchdog_and_coredumpctl_are_gaps_and_so_is_a_sysrq_without_dumps() {
        let mut inputs = inputs(true);
        inputs.coredumps = Coredumps::NotInstalled {
            core_pattern: "core".to_owned(),
        };
        inputs.sysrq = Some(176);
        let analysis = analyse(&[comp(0, "toplevel mapped")], &inputs);
        let whats: Vec<&str> = analysis.gaps.iter().map(|gap| gap.what.as_str()).collect();
        assert!(whats.iter().any(|what| what.contains("main thread")));
        assert!(whats.iter().any(|what| what.contains("core dump")));
        assert!(whats.iter().any(|what| what.contains("SysRq")));

        inputs.sysrq = Some(184);
        let watched = analyse(
            &[comp(
                0,
                "WARN templar_comp::watchdog: compositor event loop stalled stalled_ms=7004",
            )],
            &inputs,
        );
        let whats: Vec<&str> = watched.gaps.iter().map(|gap| gap.what.as_str()).collect();
        assert!(!whats.iter().any(|what| what.contains("main thread")));
        assert!(!whats.iter().any(|what| what.contains("SysRq")));
    }

    #[test]
    fn journal_json_parses_coloured_byte_array_messages_and_sorts_by_time() {
        // `ESC[32m INFO ESC[0m hi`, as `-o json` renders a message carrying control bytes.
        let text = concat!(
            r#"{"__REALTIME_TIMESTAMP":"2000","_COMM":"templar-comp","_PID":"7","MESSAGE":"#,
            "[27,91,51,50,109,32,73,78,70,79,27,91,48,109,32,104,105]}",
            "\n",
            "not json\n",
            r#"{"__REALTIME_TIMESTAMP":"1000","SYSLOG_IDENTIFIER":"kernel","_TRANSPORT":"kernel","PRIORITY":"3","MESSAGE":"Call Trace:"}"#,
            "\n",
        );
        let entries = parse_journal_json(text);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].kernel);
        assert_eq!(entries[0].priority, Some(3));
        assert_eq!(entries[1].identifier, "templar-comp");
        assert_eq!(entries[1].pid, Some(7));
        assert_eq!(entries[1].message, " INFO hi");
    }

    #[test]
    fn a_boot_offset_resolves_to_its_id_from_the_boot_list() {
        let list = "IDX BOOT ID                          FIRST ENTRY                 LAST ENTRY\n \
                    -1 a2228e0a709d40509c284f6698e0feb2 Thu 2026-10-01 00:19:11 UTC Thu 2026-10-01 01:40:32 UTC\n  \
                    0 d9b6917092c24ba582c4c6a543cc3f5e Thu 2026-10-01 01:41:52 UTC Thu 2026-10-01 01:59:34 UTC\n";
        let boot = resolve_boot("-1", list);
        assert_eq!(boot.arg(), "a2228e0a709d40509c284f6698e0feb2");
        assert_eq!(boot.label(), "a2228e0a709d40509c284f6698e0feb2");

        let unknown = resolve_boot("-7", list);
        assert_eq!(unknown.arg(), "-7");
        assert_eq!(unknown.label(), "offset-7");

        let by_id = resolve_boot("D9B6917092C24BA582C4C6A543CC3F5E", "");
        assert_eq!(by_id.arg(), "d9b6917092c24ba582c4c6a543cc3f5e");
    }

    #[test]
    fn the_effective_uid_is_the_second_uid_field() {
        let status = "Name:\ttemplar\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(effective_uid(status), Some(0));
    }
}
