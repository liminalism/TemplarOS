//! `lege-resourced` — observe-only by default.

use clap::Parser;
use lege_resourced::{
    Governor, GovernorConfig, PressureLevel,
    executor::{Mode, apply_action},
    policy::AppSample,
    psi, scan,
};
use std::{
    collections::HashMap,
    thread,
    time::{Duration, Instant},
};

/// LegeOS resource governor.
#[derive(Debug, Parser)]
#[command(name = "lege-resourced", version, about)]
struct Cli {
    /// Apply memory.high / reclaim / kill actions for real. Default is observe-only: log what
    /// would be done, until recorded traces show sensible choices.
    #[arg(long)]
    enforce: bool,

    /// Sampling interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,

    /// Run one tick and exit (for smoke tests and `lege doctor`).
    #[arg(long)]
    once: bool,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let mode = if cli.enforce {
        Mode::Enforce
    } else {
        Mode::ObserveOnly
    };
    tracing::info!(?mode, "lege-resourced starting");

    // Keep the governor's own pages resident: it must act while everything else is thrashing.
    lock_memory();

    let mut governor = Governor::new(GovernorConfig::default());
    let uid = uid();
    let app_slice = scan::user_app_slice(uid);
    let mut previous: HashMap<lege_types::InstanceId, (u64, Instant)> = HashMap::new();
    let managed = HashMap::new(); // populated once enforcement/enrichment lands

    loop {
        let now = Instant::now();
        let available = psi::read_mem_available().unwrap_or(u64::MAX);
        let memory = psi::read_global_memory_psi().unwrap_or_default();
        let level = governor.classify(available, memory.some.avg10, memory.full.avg10);

        let mut samples = Vec::new();
        for instance in scan::discover(&app_slice).unwrap_or_default() {
            let current = instance.cgroup.memory_current().unwrap_or(0);
            let growth = previous.get(&instance.instance_id).map_or(0, |(last, at)| {
                let seconds = now.saturating_duration_since(*at).as_secs_f64().max(0.001);
                ((current as f64 - *last as f64) / seconds) as i64
            });
            previous.insert(instance.instance_id, (current, now));
            let full_psi = instance
                .cgroup
                .memory_pressure()
                .ok()
                .and_then(|text| psi::parse_psi(&text).ok())
                .map_or(0.0, |p| p.full.avg10);
            samples.push(AppSample {
                instance_id: instance.instance_id,
                memory_current: current,
                memory_growth_per_second: growth,
                full_psi_avg10: full_psi,
                // Enrichment (foreground/visible from the compositor, protection/checkpoint from
                // sessiond) arrives over D-Bus signals; conservative defaults until then.
                foreground: false,
                visible: true,
                protected: false,
                reports_unsaved_state: false,
                disposable: false,
            });
        }

        tracing::info!(
            ?level,
            available_mib = available / (1024 * 1024),
            instances = samples.len(),
            "tick"
        );

        if level != PressureLevel::Normal || cli.once {
            for action in governor.decide(now, level, &samples) {
                if let Err(error) = apply_action(&managed, &action, mode) {
                    tracing::warn!(%error, "could not apply action");
                }
            }
        }

        if cli.once {
            break;
        }
        thread::sleep(Duration::from_millis(cli.interval_ms));
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    #[allow(unsafe_code)]
    unsafe {
        libc::getuid()
    }
}

fn lock_memory() {
    // SAFETY: mlockall only takes flags; failure (e.g. RLIMIT_MEMLOCK) is non-fatal.
    #[allow(unsafe_code)]
    let result = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if result != 0 {
        tracing::debug!("mlockall unavailable; continuing without locked memory");
    }
}
