//! Orchestration: the sampling loop (a plain OS thread, sync, no tokio — ADR-0002) and the D-Bus
//! bootstrap that bridges it to `crate::dbus::ResourcesApi`.
//!
//! [`tick`] is the one function that reads PSI/cgroups, decides, applies, records a trace and
//! publishes a [`Report`] — used identically by `--once` (called once, synchronously, no bus at
//! all) and by the long-running loop (called every `interval`, on its own thread). Signals cross
//! from that thread to the tokio-hosted D-Bus connection over a plain channel (`SignalEvent`):
//! sending into an unbounded `tokio::sync::mpsc` channel is itself a synchronous, non-blocking
//! call, so the loop thread never awaits anything and a stalled bus can never delay it.

use crate::{
    config,
    dbus::ResourcesApi,
    executor::{self, ManagedInstance},
    nonmemory::{self, GpuResetWatcher, KmsgWatcher, Stall},
    policy::{Action, Governor, GovernorConfig, PressureLevel, Stage},
    protect, psi, scan,
    state::{InstanceReport, Report, SharedState},
    trace::{self, ActionTrace, InstanceTrace, TraceRecord, TraceRing},
};
use anyhow::{Context, Result};
use lege_types::{InstanceId, dbus::resources as names};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use zbus::object_server::SignalEmitter;

/// Filesystem roots for one tick — always overridable, never hard-coded past this point (rule:
/// every `/proc`/`/sys/fs/cgroup` read lives behind a parameter so tests use fixture trees).
#[derive(Clone, Debug)]
pub struct TickPaths {
    pub proc_root: PathBuf,
    pub app_slice: PathBuf,
    pub session_slice: PathBuf,
    pub drm_root: PathBuf,
    pub kmsg_path: PathBuf,
}

impl TickPaths {
    pub fn production(uid: u32) -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            app_slice: scan::user_app_slice(uid),
            session_slice: scan::user_session_slice(uid),
            drm_root: PathBuf::from("/sys/class/drm"),
            kmsg_path: PathBuf::from("/dev/kmsg"),
        }
    }
}

/// Everything the sampling loop owns privately across ticks.
#[derive(Debug)]
pub struct TickContext {
    governor: Governor,
    managed: HashMap<InstanceId, ManagedInstance>,
    gpu_watcher: GpuResetWatcher,
    kmsg_watcher: KmsgWatcher,
    started_at: Instant,
    paths: TickPaths,
    /// Why the loop is awake for this tick. Set by [`run_loop`] before each [`tick`] rather than
    /// passed as an argument, so no policy code, test or replay caller changes shape.
    wakeup: crate::psi::Wakeup,
}

impl TickContext {
    pub fn new(config: GovernorConfig, paths: TickPaths) -> Self {
        Self {
            governor: Governor::new(config),
            managed: HashMap::new(),
            gpu_watcher: GpuResetWatcher::new(paths.drm_root.clone()),
            kmsg_watcher: KmsgWatcher::open(&paths.kmsg_path),
            started_at: Instant::now(),
            paths,
            wakeup: crate::psi::Wakeup::default(),
        }
    }

    pub fn managed_count(&self) -> usize {
        self.managed.len()
    }

    /// Record why this tick is happening, so the trace says whether the governor was woken by
    /// pressure or by the clock.
    pub fn note_wakeup(&mut self, wakeup: crate::psi::Wakeup) {
        self.wakeup = wakeup;
    }
}

/// One decided action, resolved to whether it was actually performed.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionOutcome {
    pub kind: &'static str,
    pub instance_id: InstanceId,
    pub applied: bool,
    pub reason: String,
}

/// What one tick produced, for the caller (loop or `--once`) to disseminate.
#[derive(Debug)]
pub struct TickOutcome {
    pub level: PressureLevel,
    pub level_changed: bool,
    pub report: Report,
    pub actions: Vec<ActionOutcome>,
    pub stalls: Vec<Stall>,
}

/// Read pressure, discover instances, decide, apply (subject to `shared`'s current stage),
/// record a trace when warranted, and publish a fresh [`Report`]. Never blocks on anything but
/// plain, local filesystem reads/writes — no D-Bus, no network, and every write is the
/// non-blocking cgroup path (`Cgroup::write_control`) or a best-effort trace append.
pub fn tick(ctx: &mut TickContext, shared: &SharedState, now: Instant) -> TickOutcome {
    let stage = shared.stage();

    let memory_psi = psi::read_global_memory_psi(&ctx.paths.proc_root).unwrap_or_default();
    let io_psi = psi::read_global_io_psi(&ctx.paths.proc_root).unwrap_or_default();
    let available = psi::read_mem_available(&ctx.paths.proc_root).unwrap_or(u64::MAX);

    let previous_level = ctx.governor.current_level();
    let level = ctx
        .governor
        .tick_level(available, memory_psi.some.avg10, memory_psi.full.avg10);
    let level_changed = level != previous_level;

    // Protect the interface explicitly (planning doc 01 §5): best-effort, every tick, so a unit
    // that starts after `lege-resourced` still gets protected on the next pass.
    protect::apply(&ctx.paths.session_slice);

    apply_pending_forgets(ctx, shared);
    let discovered = register_and_reap(ctx, shared, now);
    let (samples, instance_reports) = build_samples(ctx, shared, &discovered, now);

    // The plan, not just its actions: the reasons and the expected relief are what make a
    // recorded run reviewable, and reviewing recorded runs is the condition on raising the stage.
    let plan = ctx.governor.plan(now, level, &samples);
    if level != PressureLevel::Normal {
        for step in &plan.steps {
            tracing::debug!(
                instance_id = %step.instance_id,
                expects_mib = step.expected_relief_bytes / (1024 * 1024),
                deficit_mib = plan.deficit_bytes / (1024 * 1024),
                reason = %step.reason,
                "reclaim step",
            );
        }
    }
    let decided = plan.actions();
    let victim = (level != PressureLevel::Normal)
        .then(|| decided.first().map(Action::instance_id))
        .flatten();
    let actions = apply_decided_actions(ctx, &decided, stage);

    let mut stalls = Vec::new();
    stalls.extend(nonmemory::detect_storage_stall(
        io_psi,
        memory_psi,
        ctx.governor.config().elevated_some_avg10,
    ));
    stalls.extend(ctx.gpu_watcher.poll());
    stalls.extend(ctx.kmsg_watcher.poll());

    let report = Report {
        level,
        stage,
        uptime_seconds: now.saturating_duration_since(ctx.started_at).as_secs(),
        mem_available_bytes: available,
        global_memory_psi: memory_psi,
        global_io_psi: io_psi,
        instances: instance_reports,
    };
    *shared.report.write() = Some(report.clone());

    record_trace(shared, &report, level_changed, victim, &actions, ctx.wakeup);

    TickOutcome {
        level,
        level_changed,
        report,
        actions,
        stalls,
    }
}

fn apply_pending_forgets(ctx: &mut TickContext, shared: &SharedState) {
    let forgotten: HashSet<InstanceId> = std::mem::take(&mut shared.pending_forgets.lock());
    for id in forgotten {
        if let Some(managed) = ctx.managed.remove(&id)
            && managed.active_override.is_some()
        {
            // Only restore what the governor itself overrode — never write to a cgroup it never
            // touched (same guard as `Action::RestoreDynamicLimits`; see `executor::apply_action`).
            let _ = managed
                .cgroup
                .set_memory_high(managed.configured_memory_high);
        }
    }
}

/// Scan `app.slice`, register newly-seen instances (applying `memory.oom.group=1` once each) and
/// reap ones whose cgroup is gone. Returns the freshly discovered set the rest of the tick works
/// from. A failed scan keeps the previous managed set untouched rather than reaping everyone.
fn register_and_reap(
    ctx: &mut TickContext,
    shared: &SharedState,
    now: Instant,
) -> Vec<scan::DiscoveredInstance> {
    let (discovered, scan_ok) = match scan::discover(&ctx.paths.app_slice) {
        Ok(list) => (list, true),
        Err(error) => {
            tracing::warn!(%error, "app.slice scan failed; keeping the previous managed set");
            (Vec::new(), false)
        }
    };

    for instance in &discovered {
        let managed = ctx.managed.entry(instance.instance_id).or_insert_with(|| {
            ManagedInstance::new(instance.app_id.clone(), instance.cgroup.clone(), now)
        });
        if !managed.oom_group_applied {
            // Whole-workload termination if the kernel OOM killer ever fires in this cgroup,
            // rather than leaving helper-process wreckage behind (planning doc 02 §5).
            match managed.cgroup.set_group_oom(true) {
                Ok(()) => managed.oom_group_applied = true,
                Err(error) => {
                    tracing::warn!(instance_id = %instance.instance_id, %error, "could not apply memory.oom.group");
                }
            }
        }
    }

    if scan_ok {
        let discovered_ids: HashSet<InstanceId> = discovered
            .iter()
            .map(|instance| instance.instance_id)
            .collect();
        ctx.managed.retain(|id, managed| {
            let keep = discovered_ids.contains(id);
            if !keep {
                tracing::debug!(instance_id = %id, app_id = %managed.app_id, "instance departed");
                shared.enrichment.write().remove(id);
            }
            keep
        });
    }

    discovered
}

/// How long ago `then_ms` was, or `None` if nobody said.
///
/// A timestamp in the future yields `Duration::ZERO` rather than `None`: a clock that stepped
/// backwards should read as "just now", not as "unknown", because the event did happen.
fn age_since(then_ms: Option<i64>, now_ms: i64) -> Option<std::time::Duration> {
    let then_ms = then_ms?;
    let millis = now_ms.saturating_sub(then_ms).max(0);
    Some(std::time::Duration::from_millis(
        u64::try_from(millis).unwrap_or(0),
    ))
}

/// Sample every discovered instance's memory/growth/PSI, merge in its D-Bus enrichment, and
/// score it — the shared basis for both `Governor::decide`'s input and the published `Report`.
fn build_samples(
    ctx: &mut TickContext,
    shared: &SharedState,
    discovered: &[scan::DiscoveredInstance],
    now: Instant,
) -> (Vec<crate::policy::AppSample>, Vec<InstanceReport>) {
    let weights = ctx.governor.config().victim_weights;
    // One clock reading for the whole tick: every instance's claim is aged against the same
    // instant, so two applications that reported at the same moment cannot expire at different
    // ones. `now_ms` is a vDSO read, not a syscall — safe on this loop (ADR-0002).
    let now_ms = trace::now_ms();
    let mut samples = Vec::with_capacity(discovered.len());
    let mut instance_reports = Vec::with_capacity(discovered.len());

    for instance in discovered {
        let Some(managed) = ctx.managed.get_mut(&instance.instance_id) else {
            continue;
        };
        let current = managed.cgroup.memory_current().unwrap_or(0);
        let growth = managed.record_sample(current, now);
        let full_psi_avg10 = managed
            .cgroup
            .memory_pressure()
            .ok()
            .and_then(|text| psi::parse_psi(&text).ok())
            .map_or(0.0, |psi| psi.full.avg10);
        let enrichment = shared.enrichment_for(instance.instance_id);

        // The claim expires. Until something reports the transition back to saved, an
        // unbounded claim would excuse an application from containment for the rest of the
        // session on the strength of one message.
        let claim_current = crate::policy::unsaved_claim_is_current(
            enrichment.unsaved_since_ms,
            now_ms,
            ctx.governor.config().unsaved_ttl,
        );
        let sample = crate::policy::AppSample {
            instance_id: instance.instance_id,
            memory_current: current,
            memory_growth_per_second: growth,
            full_psi_avg10,
            foreground: enrichment.foreground,
            visible: enrichment.visible,
            protected: enrichment.protected,
            reports_unsaved_state: enrichment.has_unsaved_work && claim_current,
            disposable: enrichment.disposable,
            last_interaction_age: age_since(enrichment.last_interaction_ms, now_ms),
            project_important: enrichment.project_important,
            supports_checkpoint: enrichment.supports_checkpoint,
            checkpoint_age: enrichment
                .checkpoint_ok
                .then(|| age_since(enrichment.checkpoint_at_ms, now_ms))
                .flatten(),
            user_pinned: enrichment.user_pinned,
        };
        let breakdown = crate::policy::victim_score_breakdown(&sample, &weights);
        instance_reports.push(InstanceReport {
            instance_id: instance.instance_id,
            app_id: instance.app_id.clone(),
            memory_current_bytes: current,
            memory_growth_bytes_per_second: growth,
            full_psi_avg10,
            foreground: sample.foreground,
            visible: sample.visible,
            protected: sample.protected,
            disposable: sample.disposable,
            has_unsaved_work: sample.reports_unsaved_state,
            unsaved_work_age_ms: enrichment
                .unsaved_since_ms
                .map(|since| now_ms.saturating_sub(since)),
            last_interaction_age_ms: sample
                .last_interaction_age
                .map(|age| age.as_millis() as u64),
            project_important: sample.project_important,
            supports_checkpoint: sample.supports_checkpoint,
            checkpoint_age_ms: sample.checkpoint_age.map(|age| age.as_millis() as u64),
            user_pinned: sample.user_pinned,
            victim_score: breakdown.total,
            score_breakdown: breakdown,
        });
        samples.push(sample);
    }

    (samples, instance_reports)
}

/// Apply every decided action (subject to the current stage) and describe each one for the
/// signal/trace layer.
fn apply_decided_actions(
    ctx: &mut TickContext,
    decided: &[Action],
    stage: Stage,
) -> Vec<ActionOutcome> {
    let mut actions = Vec::with_capacity(decided.len());
    for action in decided {
        // Fetched before the mutable `apply_action` call below (it may clear the instance's
        // `active_override`, but never its `app_id`); cloned rather than borrowed so the two
        // calls don't fight the borrow checker over `ctx.managed`.
        let app_id = ctx
            .managed
            .get(&action.instance_id())
            .map(|m| m.app_id.clone());
        let applied = match executor::apply_action(&mut ctx.managed, action, stage) {
            Ok(applied) => applied,
            Err(error) => {
                tracing::warn!(?action, %error, "applying resource action failed");
                false
            }
        };
        actions.push(ActionOutcome {
            kind: action.kind(),
            instance_id: action.instance_id(),
            applied,
            reason: action_reason(action, applied, stage, app_id.as_ref()),
        });
    }
    actions
}

/// Record a tick only when it is genuinely interesting: pressure is elevated or worse, the level
/// just changed (including the return to Normal — the end of an episode is part of its story),
/// or some action actually took effect. `RestoreDynamicLimits` is decided for every running
/// instance on every Normal tick regardless of whether it does anything (see
/// `executor::apply_action`), so an idle Normal tick with a stable level and nothing applied
/// would otherwise flood the ring with roughly one no-op record per second forever — crowding out
/// the pressure episodes the ring exists to preserve for review. A containment action decided but
/// blocked by the stage only ever happens at Elevated-or-worse (every action except
/// `RestoreDynamicLimits` requires it), so that evidence is already covered by the first
/// condition without a separate check.
fn record_trace(
    shared: &SharedState,
    report: &Report,
    level_changed: bool,
    victim: Option<InstanceId>,
    actions: &[ActionOutcome],
    wakeup: crate::psi::Wakeup,
) {
    let interesting = report.level != PressureLevel::Normal
        || level_changed
        || actions.iter().any(|action| action.applied);
    if !interesting {
        return;
    }
    shared.traces.lock().append(&TraceRecord {
        trace_version: crate::replay::TRACE_VERSION,
        wakeup,
        timestamp_ms: trace::now_ms(),
        level: report.level,
        stage: report.stage,
        mem_available_bytes: report.mem_available_bytes,
        global_memory_psi: report.global_memory_psi,
        global_io_psi: report.global_io_psi,
        instances: report
            .instances
            .iter()
            .map(|instance| InstanceTrace {
                instance_id: instance.instance_id,
                app_id: instance.app_id.to_string(),
                memory_current_bytes: instance.memory_current_bytes,
                memory_growth_bytes_per_second: instance.memory_growth_bytes_per_second,
                full_psi_avg10: instance.full_psi_avg10,
                foreground: instance.foreground,
                visible: instance.visible,
                protected: instance.protected,
                disposable: instance.disposable,
                has_unsaved_work: instance.has_unsaved_work,
                last_interaction_age_ms: instance.last_interaction_age_ms,
                project_important: instance.project_important,
                supports_checkpoint: instance.supports_checkpoint,
                checkpoint_age_ms: instance.checkpoint_age_ms,
                user_pinned: instance.user_pinned,
                victim_score: instance.victim_score,
                score_breakdown: instance.score_breakdown,
            })
            .collect(),
        victim,
        actions: actions
            .iter()
            .map(|action| ActionTrace {
                kind: action.kind.to_owned(),
                instance_id: action.instance_id,
                applied: action.applied,
                reason: action.reason.clone(),
            })
            .collect(),
    });
}

fn action_reason(
    action: &Action,
    applied: bool,
    stage: Stage,
    app_id: Option<&lege_types::AppId>,
) -> String {
    let subject = app_id.map_or_else(|| action.instance_id().to_string(), ToString::to_string);

    // `RestoreDynamicLimits`'s `minimum_stage` is `Stage::ObserveOnly`, so it is never blocked by
    // the stage (see `Action::minimum_stage`) — when it is not applied, the reason is always "no
    // active override to restore", never a stage block, and the generic suffix below would be
    // actively misleading for it.
    if let Action::RestoreDynamicLimits { .. } = action {
        return if applied {
            format!("{subject} limits restored: pressure normal")
        } else {
            format!("{subject} has no active override to restore")
        };
    }

    let base = match action {
        Action::TrimCaches { .. } => {
            format!("{subject} asked to trim caches under memory pressure")
        }
        Action::SetMemoryHigh { bytes, .. } => format!(
            "{subject} throttled to {} MiB (memory.high) under memory pressure",
            bytes / crate::policy::MIB
        ),
        Action::Reclaim { bytes, .. } => {
            format!(
                "{subject} asked to proactively reclaim {} MiB",
                bytes / crate::policy::MIB
            )
        }
        Action::RequestCheckpoint { deadline, .. } => format!(
            "{subject} asked to checkpoint within {:.1}s before possible termination",
            deadline.as_secs_f64()
        ),
        Action::KillCgroup { reason, .. } => format!("{subject} terminated: {reason}"),
        Action::RestoreDynamicLimits { .. } => unreachable!("handled above"),
    };
    if applied {
        base
    } else {
        format!("{base} (decided at stage {stage}; not performed)")
    }
}

/// This machine's `MemTotal`, or the reference machine's when `/proc/meminfo` cannot be read.
///
/// Falling back to the reference rather than to zero is deliberate: zero would scale every
/// threshold to its floor and make the governor believe a large machine is a tiny one. A
/// `/proc` that cannot be read is a broken environment, and the least surprising behaviour there
/// is the behaviour this project has shipped all along.
fn read_mem_total(proc_root: &Path) -> u64 {
    psi::read_mem_total(proc_root).unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read MemTotal; using the reference machine's thresholds");
        crate::policy::REFERENCE_MEMORY
    })
}

/// Run one tick with real defaults, print the resulting report and exit. No D-Bus, no
/// persistence — this is `lege-resourced --once`, a smoke test that must succeed even on a
/// machine with no LegeOS units at all.
pub fn run_once() -> Result<()> {
    lock_memory();
    let paths = TickPaths::production(current_uid());
    let mem_total = read_mem_total(&paths.proc_root);
    let config =
        config::load_for_memory(&config::default_path(), mem_total).unwrap_or_else(|error| {
            tracing::warn!(%error, "invalid resourced.toml; using defaults");
            GovernorConfig::for_total_memory(mem_total)
        });
    let mut ctx = TickContext::new(config, paths);
    let shared = SharedState::in_memory();
    let outcome = tick(&mut ctx, &shared, Instant::now());

    println!(
        "{}",
        serde_json::to_string_pretty(&outcome.report).unwrap_or_default()
    );
    for stall in &outcome.stalls {
        tracing::warn!(?stall, "non-memory stall detected");
    }
    tracing::info!(
        level = %outcome.level,
        instances = outcome.report.instances.len(),
        actions = outcome.actions.len(),
        "tick complete"
    );
    Ok(())
}

/// How long the loop sleeps when nothing is under pressure.
///
/// It is not zero: the tick re-reads cgroup membership, and an instance appearing or leaving
/// `app.slice` produces no event anybody can wait on. A minute is long enough that an idle
/// desktop is not paying for a governor that has nothing to govern, and short enough that a
/// newly launched application is protected before it can matter.
pub const IDLE_HEARTBEAT: Duration = Duration::from_secs(60);

/// A stall of 150ms in any second is the kernel's signal that memory is starting to hurt — well
/// below the Elevated threshold the governor acts on, so the loop is awake and sampling before
/// there is anything to decide.
const TRIGGER_STALL: Duration = Duration::from_millis(150);
const TRIGGER_WINDOW: Duration = Duration::from_secs(1);

/// The long-running loop: wake on memory pressure or on the heartbeat, forever, on its own
/// thread.
///
/// It used to `sleep(interval)` unconditionally, which meant an idle machine woke this thread
/// once a second to read PSI files and discover that nothing had changed. Now the kernel says
/// when memory starts stalling and the loop waits for it; `interval` is what it samples at
/// *while* something is happening, which is the bounded active episode
/// `@legeos.decision.event-driven-service-authorities` allows.
///
/// The policy is untouched: `tick` reads the same files, `Governor` makes the same decisions, and
/// the replay fixtures prove it. What changed is only how often the loop asks.
///
/// `shared` and `events` are taken by value, not by reference: this runs inside a
/// `thread::Builder::spawn`ed closure, which requires `'static` owned captures — a borrow cannot
/// satisfy that, regardless of how little of each value the loop body needs.
#[allow(clippy::needless_pass_by_value)]
pub fn run_loop(
    mut ctx: TickContext,
    shared: Arc<SharedState>,
    interval: Duration,
    events: tokio::sync::mpsc::UnboundedSender<SignalEvent>,
) {
    let session = crate::cgroup::Cgroup::from_directory(ctx.paths.session_slice.clone());
    let wait = crate::psi::PressureWait::for_cgroup(&session, TRIGGER_STALL, TRIGGER_WINDOW);
    tracing::info!(
        event_driven = wait.is_event_driven(),
        pressure = %session.directory().join("memory.pressure").display(),
        "governor loop armed"
    );

    let mut wakeup = crate::psi::Wakeup::default();
    loop {
        ctx.note_wakeup(wakeup);
        let outcome = tick(&mut ctx, &shared, Instant::now());

        if outcome.level_changed {
            let report_json = serde_json::to_string(&outcome.report).unwrap_or_default();
            let _ = events.send(SignalEvent::PressureChanged {
                level: outcome.level.to_string(),
                report_json,
            });
        }
        for action in &outcome.actions {
            let _ = events.send(SignalEvent::PressureAction {
                kind: action.kind,
                instance_id: action.instance_id.to_string(),
                applied: action.applied,
                reason: action.reason.clone(),
                detail_json: "{}".to_owned(),
            });
        }
        for stall in &outcome.stalls {
            let detail_json = serde_json::to_string(stall).unwrap_or_default();
            let _ = events.send(SignalEvent::NonMemoryStall {
                kind: stall.kind(),
                detail_json,
            });
        }

        tracing::info!(
            level = %outcome.level,
            instances = ctx.managed_count(),
            actions = outcome.actions.len(),
            wakeup = wakeup.as_str(),
            "tick"
        );

        // Under pressure, keep the sampling interval: the governor is tracking a moving situation
        // and a trigger that has already fired says nothing more until the stall clears. Calm,
        // wait for the kernel — and the heartbeat is the backstop for the one thing PSI cannot
        // report, which is an instance appearing in or leaving the slice.
        wakeup = if outcome.level == PressureLevel::Normal {
            wait.wait(IDLE_HEARTBEAT)
        } else {
            wait.wait(interval)
        };
    }
}

/// A signal decided by the loop thread, crossing to the tokio task that actually emits it.
#[derive(Debug)]
pub enum SignalEvent {
    PressureChanged {
        level: String,
        report_json: String,
    },
    PressureAction {
        kind: &'static str,
        instance_id: String,
        applied: bool,
        reason: String,
        detail_json: String,
    },
    NonMemoryStall {
        kind: &'static str,
        detail_json: String,
    },
}

async fn dispatch_signal(emitter: &SignalEmitter<'static>, event: SignalEvent) -> zbus::Result<()> {
    match event {
        SignalEvent::PressureChanged { level, report_json } => {
            ResourcesApi::pressure_changed(emitter, level, report_json).await
        }
        SignalEvent::PressureAction {
            kind,
            instance_id,
            applied,
            reason,
            detail_json,
        } => {
            ResourcesApi::pressure_action(
                emitter,
                kind.to_owned(),
                instance_id,
                applied,
                reason,
                detail_json,
            )
            .await
        }
        SignalEvent::NonMemoryStall { kind, detail_json } => {
            ResourcesApi::non_memory_stall(emitter, kind.to_owned(), detail_json).await
        }
    }
}

/// `$XDG_STATE_HOME/legeos/resources` (falling back to `~/.local/state`, then `/var/tmp`).
pub fn default_state_dir() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    state_home.join("legeos").join("resources")
}

/// Serve `org.legeos.Resources1` and run the sampling loop on its own thread. Blocks forever.
pub async fn serve(interval: Duration, config_path: &Path, state_dir: &Path) -> Result<()> {
    // Before `lock_memory`, deliberately: the stamp is one small write to a tmpfs, and doing it
    // while the address space is still ordinary keeps the locked region to what the loop needs.
    // `--once` does not stamp — it is a smoke test, not a running governor.
    if let Err(error) = lege_build::write_stamp("lege-resourced") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    lock_memory();

    // Once, at startup: how much memory this machine has does not change while it runs, and the
    // sampling loop must not grow a file read for an answer that is always the same.
    let paths = TickPaths::production(current_uid());
    let mem_total = read_mem_total(&paths.proc_root);
    let config = config::load_for_memory(config_path, mem_total)
        .with_context(|| format!("loading {}", config_path.display()))?;
    tracing::info!(
        mem_total_mib = mem_total / (1024 * 1024),
        reserve_mib = config.reserve_bytes / (1024 * 1024),
        critical_reserve_mib = config.critical_reserve_bytes / (1024 * 1024),
        "thresholds scaled to this machine",
    );
    let traces = TraceRing::open(state_dir, 5000, 4 * 1024 * 1024);
    let stage_path = state_dir.join("stage.toml");
    let shared = Arc::new(SharedState::new(Some(stage_path), traces));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SignalEvent>();

    let connection = zbus::connection::Builder::session()?
        .name(names::BUS_NAME)?
        .serve_at(names::PATH, ResourcesApi::new(shared.clone()))?
        .build()
        .await
        .context("claiming org.legeos.Resources")?;

    let interface_ref = connection
        .object_server()
        .interface::<_, ResourcesApi>(names::PATH)
        .await
        .context("locating the just-registered Resources1 interface")?;
    let emitter = interface_ref.signal_emitter().clone();

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Err(error) = dispatch_signal(&emitter, event).await {
                tracing::warn!(%error, "failed to emit a resources signal");
            }
        }
    });

    let ctx = TickContext::new(config, TickPaths::production(current_uid()));
    let loop_shared = shared.clone();
    std::thread::Builder::new()
        .name("lege-resourced-loop".to_owned())
        .spawn(move || run_loop(ctx, loop_shared, interval, tx))
        .context("spawning the sampling loop thread")?;

    tracing::info!(name = names::BUS_NAME, "lege-resourced ready");
    std::future::pending::<()>().await;
    Ok(())
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    #[allow(unsafe_code)]
    unsafe {
        libc::getuid()
    }
}

/// Keep the governor's already-allocated pages resident: it must act while everything else is
/// thrashing (ADR-0002). `MCL_FUTURE` is deliberately excluded. With a finite `LimitMEMLOCK`,
/// successfully enabling it turns an ordinary later allocation into `ENOMEM`, which Rust's
/// allocator handles by aborting the process — exactly the opposite of keeping the governor
/// alive. Failure (for example a tight `RLIMIT_MEMLOCK`) is logged, not fatal.
pub fn lock_memory() {
    // SAFETY: mlockall only takes flags; failure is reported through its return value and is
    // non-fatal — the process continues without locked memory.
    #[allow(unsafe_code)]
    let result = unsafe { libc::mlockall(libc::MCL_CURRENT) };
    if result != 0 {
        tracing::debug!("mlockall unavailable; continuing without locked memory");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppId, unit};
    use std::fs;

    #[test]
    fn locking_current_pages_does_not_poison_future_allocations() {
        lock_memory();
        let allocation = vec![0x5a_u8; 128 * 1024];
        assert_eq!(allocation.len(), 128 * 1024);
        assert_eq!(allocation[0], 0x5a);
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lege-resourced-daemon-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn paths(&self) -> TickPaths {
            TickPaths {
                proc_root: self.dir.join("proc"),
                app_slice: self.dir.join("app.slice"),
                session_slice: self.dir.join("session.slice"),
                drm_root: self.dir.join("drm"),
                kmsg_path: self.dir.join("kmsg-missing"),
            }
        }

        fn write_proc(&self, mem_available_kib: u64, memory_full_avg10: f64) {
            let proc = self.dir.join("proc");
            fs::create_dir_all(proc.join("pressure")).unwrap();
            fs::write(
                proc.join("meminfo"),
                format!("MemAvailable: {mem_available_kib} kB\n"),
            )
            .unwrap();
            fs::write(
                proc.join("pressure/memory"),
                format!(
                    "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10={memory_full_avg10:.2} avg60=0.00 avg300=0.00 total=0\n"
                ),
            )
            .unwrap();
            fs::write(
                proc.join("pressure/io"),
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            )
            .unwrap();
        }

        fn add_app_instance(&self, app: &AppId, id: InstanceId, memory_bytes: u64) {
            let unit_dir = self.dir.join("app.slice").join(unit::unit_name(app, id));
            fs::create_dir_all(&unit_dir).unwrap();
            fs::write(unit_dir.join("memory.current"), memory_bytes.to_string()).unwrap();
            fs::write(
                unit_dir.join("memory.pressure"),
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            )
            .unwrap();
            for file in [
                "memory.high",
                "memory.max",
                "memory.oom.group",
                "memory.reclaim",
                "cgroup.kill",
            ] {
                fs::write(unit_dir.join(file), "0\n").unwrap();
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_tick_with_nothing_present_is_normal_and_produces_an_empty_report() {
        let fixture = Fixture::new("empty");
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();

        let outcome = tick(&mut ctx, &shared, Instant::now());
        assert_eq!(outcome.level, PressureLevel::Normal);
        assert!(outcome.report.instances.is_empty());
        assert!(outcome.actions.is_empty());
    }

    #[test]
    fn a_discovered_instance_gets_oom_group_applied_and_is_reaped_once_gone() {
        let fixture = Fixture::new("register-reap");
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        let app: AppId = "org.legeos.Test".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 100 * 1024 * 1024);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        let outcome = tick(&mut ctx, &shared, Instant::now());
        assert_eq!(outcome.report.instances.len(), 1);
        assert_eq!(ctx.managed_count(), 1);
        let oom_group = fs::read_to_string(
            fixture
                .dir
                .join("app.slice")
                .join(unit::unit_name(&app, id))
                .join("memory.oom.group"),
        )
        .unwrap();
        assert_eq!(oom_group.trim(), "1");

        // The instance's cgroup disappears (unit exited) — the next tick must reap it.
        fs::remove_dir_all(
            fixture
                .dir
                .join("app.slice")
                .join(unit::unit_name(&app, id)),
        )
        .unwrap();
        let outcome = tick(&mut ctx, &shared, Instant::now());
        assert!(outcome.report.instances.is_empty());
        assert_eq!(ctx.managed_count(), 0);
    }

    #[test]
    fn severe_pressure_decides_actions_that_stay_unapplied_at_observe_only() {
        let fixture = Fixture::new("severe-observe-only");
        // Below the default 3 GiB reserve: Severe.
        fixture.write_proc(2 * 1024 * 1024, 0.0); // 2 GiB available: between the 1 GiB critical and 3 GiB reserve => Severe.
        let app: AppId = "org.legeos.Hog".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 2 * crate::policy::GIB);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        assert_eq!(shared.stage(), Stage::ObserveOnly);

        let outcome = tick(&mut ctx, &shared, Instant::now());
        assert_eq!(outcome.level, PressureLevel::Severe);
        assert!(!outcome.actions.is_empty());
        assert!(outcome.actions.iter().all(|action| !action.applied));

        // Recorded even though nothing was applied: this is the evidence the ladder depends on.
        let traces = shared.traces.lock().recent(10);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].contains("\"severe\""));
    }

    #[test]
    fn throttle_stage_actually_writes_memory_high() {
        let fixture = Fixture::new("throttle-applies");
        fixture.write_proc(2 * 1024 * 1024, 0.0); // 2 GiB available: between the 1 GiB critical and 3 GiB reserve => Severe.
        let app: AppId = "org.legeos.Hog".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 2 * crate::policy::GIB);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        shared.set_stage(Stage::Throttle);

        let outcome = tick(&mut ctx, &shared, Instant::now());
        assert!(
            outcome
                .actions
                .iter()
                .any(|a| a.kind == "set-memory-high" && a.applied)
        );
        let high = fs::read_to_string(
            fixture
                .dir
                .join("app.slice")
                .join(unit::unit_name(&app, id))
                .join("memory.high"),
        )
        .unwrap();
        assert_ne!(high.trim(), "0");
    }

    #[test]
    fn forget_instance_restores_limits_on_the_next_tick() {
        let fixture = Fixture::new("forget");
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        let app: AppId = "org.legeos.Test".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 10 * 1024 * 1024);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        tick(&mut ctx, &shared, Instant::now());
        assert_eq!(ctx.managed_count(), 1);

        shared.pending_forgets.lock().insert(id);
        // Still discoverable on disk (unlike the reap test): forgetting must win even though the
        // instance is still technically present, because the caller (D-Bus) said to drop it.
        tick(&mut ctx, &shared, Instant::now());
        assert_eq!(
            ctx.managed_count(),
            1,
            "rediscovered on the same tick it was forgotten"
        );
    }

    #[test]
    fn explain_instance_matches_what_the_tick_published() {
        let fixture = Fixture::new("explain");
        fixture.write_proc(2 * 1024 * 1024, 0.0); // 2 GiB available: between the 1 GiB critical and 3 GiB reserve => Severe.
        let app: AppId = "org.legeos.Hog".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 2 * crate::policy::GIB);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        tick(&mut ctx, &shared, Instant::now());

        let report_guard = shared.report.read();
        let explanation = report_guard.as_ref().unwrap().explain(id);
        assert!(explanation.known);
        assert!(explanation.would_be_selected);
    }

    /// Coordinator-flagged defect 1: `RestoreDynamicLimits` is decided for every running instance
    /// on every Normal-pressure tick regardless of stage (`Governor::decide`). Without the
    /// `active_override` guard in `executor::apply_action`, the default (observe-only) stage
    /// would `open(O_NONBLOCK) + write` `memory.high` into every `app.slice` cgroup once per
    /// second forever — exactly the enforcement-shaped write "observe-only" promises never
    /// happens.
    #[test]
    fn observe_only_never_writes_memory_high_with_an_application_present() {
        let fixture = Fixture::new("observe-only-no-write");
        fixture.write_proc(16 * 1024 * 1024, 0.0); // plenty available => Normal.
        let app: AppId = "org.legeos.Idle".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 50 * 1024 * 1024);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        assert_eq!(shared.stage(), Stage::ObserveOnly);

        let high_path = fixture
            .dir
            .join("app.slice")
            .join(unit::unit_name(&app, id))
            .join("memory.high");
        let before = fs::read_to_string(&high_path).unwrap();

        for _ in 0..5 {
            let outcome = tick(&mut ctx, &shared, Instant::now());
            assert_eq!(outcome.level, PressureLevel::Normal);
            // RestoreDynamicLimits is decided every Normal tick for the one running instance;
            // it must never actually be performed when nothing overrode it.
            assert!(!outcome.actions.is_empty());
            assert!(outcome.actions.iter().all(|action| !action.applied));
        }

        let after = fs::read_to_string(&high_path).unwrap();
        assert_eq!(
            before, after,
            "memory.high must never be written when nothing overrode it"
        );
    }

    /// Coordinator-flagged defect 2: an idle Normal tick's only "action" is a no-op
    /// `RestoreDynamicLimits` — that must not be treated as evidence worth keeping, or the
    /// bounded trace ring fills with roughly one no-op record per second and rotates out the
    /// pressure episodes it exists to preserve.
    #[test]
    fn a_normal_idle_tick_appends_no_trace() {
        let fixture = Fixture::new("normal-idle-no-trace");
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        let app: AppId = "org.legeos.Idle".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 50 * 1024 * 1024);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();

        for _ in 0..5 {
            let outcome = tick(&mut ctx, &shared, Instant::now());
            assert_eq!(outcome.level, PressureLevel::Normal);
        }

        assert!(shared.traces.lock().recent(10).is_empty());
    }

    /// The flip side of both fixes: a real, previously-applied override must still be restored
    /// (written, not skipped) once pressure genuinely clears, and that restoration is itself
    /// evidence worth a trace record even though the tick that produces it is Normal.
    #[test]
    fn restoring_a_real_override_still_writes_and_is_traced() {
        let fixture = Fixture::new("restore-active-override");
        let app: AppId = "org.legeos.Hog".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 2 * crate::policy::GIB);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory();
        shared.set_stage(Stage::Throttle);

        // Severe: SetMemoryHigh is applied, recording a real active override.
        fixture.write_proc(2 * 1024 * 1024, 0.0);
        tick(&mut ctx, &shared, Instant::now());
        let high_path = fixture
            .dir
            .join("app.slice")
            .join(unit::unit_name(&app, id))
            .join("memory.high");
        assert_ne!(fs::read_to_string(&high_path).unwrap().trim(), "0");

        // Pressure clears; hysteresis needs `hysteresis_downgrade_ticks` (default 3) consecutive
        // good ticks before the effective level actually drops back to Normal.
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        tick(&mut ctx, &shared, Instant::now());
        tick(&mut ctx, &shared, Instant::now());
        let outcome = tick(&mut ctx, &shared, Instant::now());

        assert_eq!(outcome.level, PressureLevel::Normal);
        assert!(
            outcome
                .actions
                .iter()
                .any(|action| action.kind == "restore-limits" && action.applied)
        );
        assert_eq!(fs::read_to_string(&high_path).unwrap().trim(), "max");

        // A real event (pressure resolved, override lifted) must be traced even at Normal.
        assert!(!shared.traces.lock().recent(10).is_empty());
    }

    /// Isolates the third disjunct of `record_trace`'s "interesting" check from the other two:
    /// in pure observe-only nothing is *ever* applied (`SetMemoryHigh`'s minimum stage is
    /// `Throttle`, so no override is ever set to later restore), so the return-to-Normal tick
    /// that ends a pressure episode must be recorded on `level_changed` alone.
    #[test]
    fn observe_only_still_traces_the_end_of_an_episode_though_nothing_was_ever_applied() {
        let fixture = Fixture::new("observe-only-episode-end");
        let app: AppId = "org.legeos.Hog".parse().unwrap();
        let id = InstanceId::new();
        fixture.add_app_instance(&app, id, 2 * crate::policy::GIB);

        let mut ctx = TickContext::new(GovernorConfig::default(), fixture.paths());
        let shared = SharedState::in_memory(); // default stage: observe-only
        assert_eq!(shared.stage(), Stage::ObserveOnly);

        // Severe.
        fixture.write_proc(2 * 1024 * 1024, 0.0);
        tick(&mut ctx, &shared, Instant::now());

        // Pressure clears; hysteresis needs 3 consecutive good ticks before the level actually
        // drops back to Normal.
        fixture.write_proc(16 * 1024 * 1024, 0.0);
        tick(&mut ctx, &shared, Instant::now());
        tick(&mut ctx, &shared, Instant::now());
        let outcome = tick(&mut ctx, &shared, Instant::now());

        assert_eq!(outcome.level, PressureLevel::Normal);
        assert!(
            outcome.actions.iter().all(|action| !action.applied),
            "observe-only never applies anything"
        );

        // The end of the episode is still worth a trace record even though nothing was applied.
        assert!(!shared.traces.lock().recent(10).is_empty());
    }
}
