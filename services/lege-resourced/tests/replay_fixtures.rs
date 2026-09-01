//! The checked-in replay fixtures, against the policy in this build.
//!
//! This is the gate the roadmap puts every later policy change behind: change a weight or a
//! threshold and this test says, in the diff's own words, which decisions moved. Making it pass
//! again means blessing the fixtures — deliberately, with the diff quoted in the commit — rather
//! than editing an expectation until it matches.
//!
//! The fixtures are hand-authored scenarios, not recordings of anyone's session; see their
//! README. A green run here is evidence about *stability*, never about calibration.

use lege_resourced::{policy::GovernorConfig, replay};
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/resources/replay")
}

#[test]
fn the_checked_in_traces_replay_identically_through_this_build() {
    let dir = fixtures();
    assert!(dir.is_dir(), "replay fixtures missing at {}", dir.display());
    let reports = replay::replay_path(&dir, GovernorConfig::default()).expect("reading fixtures");
    assert_eq!(reports.len(), 4, "four scenarios are checked in");

    let mut diff = String::new();
    for report in &reports {
        assert!(report.records > 0, "{} is empty", report.source);
        assert_eq!(
            report.unreadable, 0,
            "{} has unreadable lines",
            report.source
        );
        diff.push_str(&report.render());
    }
    assert!(
        reports.iter().all(replay::ReplayReport::agrees),
        "the policy no longer decides what these traces recorded. If that is deliberate, read \
         this diff, quote it in the commit, and re-bless the fixtures with \
         `cargo run -p lege-resourced -- --replay tests/fixtures/resources/replay --bless`:\n\n{diff}"
    );
}

#[test]
fn the_fixtures_still_cover_the_four_situations_they_were_written_for() {
    // A fixture that has quietly stopped exercising the thing it was written for is worse than
    // no fixture: it is a green test standing in for coverage that is gone.
    let reports = replay::replay_path(&fixtures(), GovernorConfig::default()).expect("fixtures");
    let names: Vec<&str> = reports.iter().map(|r| r.source.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "01-calm-desktop.jsonl",
            "02-elevated-background-build.jsonl",
            "03-severe-foreground-runaway.jsonl",
            "04-critical-then-recovery.jsonl",
        ]
    );

    let text = std::fs::read_to_string(fixtures().join("04-critical-then-recovery.jsonl")).unwrap();
    assert!(
        text.contains("kill-cgroup"),
        "the critical fixture must still reach the emergency decision"
    );
    assert!(
        text.contains("\"critical\"") && text.contains("\"normal\""),
        "the critical fixture must still cover both the escalation and the recovery"
    );

    let calm = std::fs::read_to_string(fixtures().join("01-calm-desktop.jsonl")).unwrap();
    assert!(
        !calm.contains("trim-caches") && !calm.contains("kill-cgroup"),
        "a calm desktop must still provoke nothing"
    );
}
