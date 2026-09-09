//! The checked-in replay fixtures, against the policy in this build.
//!
//! This is the gate the roadmap puts every later policy change behind: change a weight or a
//! threshold and this test says, in the diff's own words, which decisions moved. Making it pass
//! again means blessing the fixtures — deliberately, with the diff quoted in the commit — rather
//! than editing an expectation until it matches.
//!
//! The fixtures are hand-authored scenarios, not recordings of anyone's session; see their
//! README. A green run here is evidence about *stability*, never about calibration.

use lege_resourced::{
    policy::{GIB, GovernorConfig, VictimWeights},
    replay,
};
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/resources/replay")
}

#[test]
fn the_checked_in_traces_replay_identically_through_this_build() {
    let dir = fixtures();
    assert!(dir.is_dir(), "replay fixtures missing at {}", dir.display());
    let reports = replay::replay_path(&dir, GovernorConfig::default()).expect("reading fixtures");
    assert_eq!(reports.len(), 9, "nine scenarios are checked in");

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
fn the_fixtures_still_cover_the_situations_they_were_written_for() {
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
            "05-low-memory-no-stall.jsonl",
            "06-attention-and-intent.jsonl",
            "07-pressure-trigger-wakeup.jsonl",
            "08-no-trigger-fallback-sampling.jsonl",
            "09-critical-victim-change.jsonl",
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

#[test]
fn the_same_traces_decide_differently_on_a_small_machine() {
    // The evidence that scaling the thresholds does something, produced the way the roadmap asks
    // for it: replay the recorded inputs through a different configuration and read the diff,
    // rather than asserting a number and hoping it means what it looks like.
    //
    // `05-low-memory-no-stall` is the fixture that isolates the memory leg: 2 GiB free and no
    // stall at all. On the 48 GiB reference machine that is below the 3 GiB reserve and reads as
    // Severe. Through a 4 GiB laptop's scaled thresholds — a 768 MiB reserve — 2 GiB free is
    // half the machine and reads as Normal, which is the false alarm the fixed constants produced
    // on every small machine, forever, from the login screen onward.
    let laptop = GovernorConfig::for_total_memory(4 * GIB);
    let reports = replay::replay_path(&fixtures(), laptop).expect("fixtures");
    let severe = reports
        .iter()
        .find(|report| report.source.starts_with("05-"))
        .expect("the low-memory fixture");
    assert!(
        !severe.agrees(),
        "a 4 GiB laptop must not reach the same conclusions from 2 GiB free as a 48 GiB \
         workstation does:\n{}",
        severe.render()
    );
    assert!(
        severe
            .divergences
            .iter()
            .any(replay::Divergence::level_changed),
        "the divergence must be about the pressure level:\n{}",
        severe.render()
    );
}

#[test]
fn attention_and_intent_change_which_instance_is_chosen() {
    // The evidence for the richer signals, produced the same way as the memory-scaling one:
    // replay the recorded inputs through a configuration where the new weights are zero — which
    // is the policy as it stood before them — and read the diff.
    //
    // `06-attention-and-intent` is a Severe tick with three candidates: a document the user is
    // typing in, the biggest process on the machine which the user pinned, and a terminal
    // untouched for three quarters of an hour that can be checkpointed and restarted. With the
    // new signals the terminal is chosen. Without them the browser is, on size alone — the user
    // having asked to keep it, and the terminal having been abandoned for forty-five minutes,
    // being facts the score could not see.
    let before = GovernorConfig {
        victim_weights: VictimWeights {
            idle_bonus_per_minute: 0.0,
            idle_bonus_cap: 0.0,
            project_protection: 0.0,
            checkpoint_capable_bonus: 0.0,
            fresh_checkpoint_bonus: 0.0,
            pinned_protection: 0.0,
            ..VictimWeights::default()
        },
        ..GovernorConfig::default()
    };
    let reports = replay::replay_path(&fixtures(), before).expect("fixtures");
    let attention = reports
        .iter()
        .find(|report| report.source.starts_with("06-"))
        .expect("the attention fixture");
    assert!(
        !attention.agrees(),
        "the new weights must change something, or they are decoration:\n{}",
        attention.render()
    );
    assert!(
        attention
            .divergences
            .iter()
            .all(replay::Divergence::victim_changed),
        "the change should be about who is chosen, not about the pressure level:\n{}",
        attention.render()
    );

    // And the other five are untouched by the same configuration: none of them carries an
    // attention or intent signal, so zeroing those weights can have no effect on them.
    for report in &reports {
        if !report.source.starts_with("06-") {
            assert!(report.agrees(), "{}", report.render());
        }
    }
}

#[test]
fn the_grace_period_follows_the_victim_rather_than_the_episode() {
    // `09` is one unbroken Critical episode in which the first victim is terminated and vanishes
    // while the pressure stays. The tick that first selects the second victim must ask it to
    // checkpoint and must *not* kill it: the emergency delay is that application's chance to save
    // its work, so it belongs to whoever was chosen, not to how long the machine has been in
    // trouble. Bound to the episode instead, the second victim was asked to checkpoint and killed
    // in the same tick.
    let records: Vec<lege_resourced::trace::TraceRecord> =
        std::fs::read_to_string(fixtures().join("09-critical-victim-change.jsonl"))
            .expect("09")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a fixture record"))
            .collect();

    let kinds = |record: &lege_resourced::trace::TraceRecord| -> Vec<String> {
        record
            .actions
            .iter()
            .map(|action| action.kind.clone())
            .collect()
    };

    let first_victim = records[0].victim.expect("a victim from the first tick");
    let transition = records
        .iter()
        .position(|record| record.victim.is_some_and(|victim| victim != first_victim))
        .expect("the fixture must change victim");

    assert!(
        kinds(&records[transition - 1]).contains(&"kill-cgroup".to_owned()),
        "the first victim's own delay must have elapsed before it vanishes"
    );
    assert!(
        records[transition]
            .instances
            .iter()
            .all(|instance| instance.instance_id != first_victim),
        "the first victim is gone by the tick the second one is chosen"
    );
    assert_eq!(records[transition].level.to_string(), "critical");
    let at_transition = kinds(&records[transition]);
    assert!(at_transition.contains(&"request-checkpoint".to_owned()));
    assert!(
        !at_transition.contains(&"kill-cgroup".to_owned()),
        "the second victim inherited the first one's expired countdown: {at_transition:?}"
    );
    assert!(
        kinds(records.last().expect("records")).contains(&"kill-cgroup".to_owned()),
        "the second victim must still be killed once its own delay elapses"
    );
}

#[test]
fn why_the_loop_woke_is_recorded_and_changes_nothing_it_decides() {
    // `07` and `08` are the same episode — the same memory, the same PSI, the same instances —
    // recorded on a kernel that offers a pressure trigger and on one that does not. The governor
    // must reach identical conclusions from both: `wakeup` says why the loop was awake, which is
    // a diagnostic about the *service*, and it must never become an input to policy.
    let reports = replay::replay_path(&fixtures(), GovernorConfig::default()).expect("fixtures");
    let triggered = reports
        .iter()
        .find(|report| report.source.starts_with("07-"))
        .expect("the trigger fixture");
    let fallback = reports
        .iter()
        .find(|report| report.source.starts_with("08-"))
        .expect("the fallback fixture");

    assert!(triggered.agrees() && fallback.agrees());
    assert_eq!(
        triggered.records, fallback.records,
        "the two fixtures must be the same episode"
    );

    let with_trigger =
        std::fs::read_to_string(fixtures().join("07-pressure-trigger-wakeup.jsonl")).expect("07");
    let without = std::fs::read_to_string(fixtures().join("08-no-trigger-fallback-sampling.jsonl"))
        .expect("08");
    assert!(
        with_trigger.contains("\"wakeup\":\"trigger\"")
            && with_trigger.contains("\"wakeup\":\"timeout\""),
        "07 must cover both a pressure-triggered wake and the idle heartbeat"
    );
    assert!(
        !without.contains("\"wakeup\":\"trigger\""),
        "08 is the kernel with no trigger; every wake there is an interval sample"
    );
    // Strip the field that is meant to differ, and nothing else may.
    let strip = |text: &str| {
        text.replace("\"wakeup\":\"trigger\"", "")
            .replace("\"wakeup\":\"timeout\"", "")
            .replace("\"wakeup\":\"fallback\"", "")
    };
    assert_eq!(
        strip(&with_trigger),
        strip(&without),
        "the two fixtures differ in something other than why the loop woke"
    );
}
