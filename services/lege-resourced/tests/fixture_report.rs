//! One end-to-end tick against a checked-in fixture tree (`tests/fixtures/resources/`), as
//! opposed to the per-scenario tempdir fixtures the unit tests build on the fly: this is the
//! literal "fixture tree under tests/fixtures/" rule 4 asks for, exercised as a black box through
//! the crate's public API — no root, no real cgroup or `/proc` access.

use lege_resourced::{
    daemon::{TickContext, TickPaths, tick},
    policy::{GovernorConfig, PressureLevel},
    state::SharedState,
};
use std::{path::PathBuf, time::Instant};

fn fixture_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is services/lege-resourced; the shared fixture tree lives at the repo
    // root's tests/fixtures/, two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/resources")
}

#[test]
fn a_tick_against_the_checked_in_fixture_tree_reports_severe_pressure_observe_only() {
    let root = fixture_root();
    assert!(root.is_dir(), "fixture tree missing at {}", root.display());

    let paths = TickPaths {
        proc_root: root.join("proc"),
        app_slice: root.join("cgroup/app.slice"),
        session_slice: root.join("cgroup/session.slice"),
        drm_root: root.join("cgroup/drm-absent"),
        kmsg_path: root.join("cgroup/kmsg-absent"),
    };
    let mut ctx = TickContext::new(GovernorConfig::default(), paths);
    let shared = SharedState::in_memory();

    let outcome = tick(&mut ctx, &shared, Instant::now());

    // MemAvailable (2 GiB) is between the 1 GiB critical and 3 GiB reserve, and full PSI avg10
    // (1.10) clears the 0.5 severe bar: Severe by either leg.
    assert_eq!(outcome.level, PressureLevel::Severe);

    assert_eq!(outcome.report.instances.len(), 1);
    let waterfox = &outcome.report.instances[0];
    assert_eq!(waterfox.app_id.as_str(), "org.legeos.Waterfox");
    assert!(waterfox.memory_current_bytes > 13 * 1024 * 1024 * 1024);

    // Severe decides SetMemoryHigh/Reclaim/TrimCaches for the one unprotected instance, but the
    // default stage is observe-only: decided, signalled, never written.
    assert!(!outcome.actions.is_empty());
    assert!(outcome.actions.iter().all(|action| !action.applied));
    assert!(
        outcome
            .actions
            .iter()
            .any(|action| action.instance_id == waterfox.instance_id)
    );

    // This is exactly the evidence the staged-enablement ladder depends on: a Severe tick with a
    // real target must leave a trace behind even though nothing was actually applied.
    let traces = shared.traces.lock().recent(10);
    assert_eq!(traces.len(), 1);
    assert!(traces[0].contains("\"severe\""));
    assert!(traces[0].contains("org.legeos.Waterfox"));
}
