//! The protected pressure overlay's content — a pure function from typed inputs to the sentence
//! `docs/architecture/resources.md` and planning doc 01 §"System-wide change history" give as the
//! target: "Waterfox was terminated at 14:32 after its application group reached 13.4 GiB and
//! caused sustained full memory stalls. Session state was saved. Restore?"
//!
//! The compositor core owns the overlay surface itself (`interfaces/wayland/lege-recovery-v1.xml`
//! — draft, no client, not even the trusted shell, needs to ask for it over the wire); this
//! module is the text the shell would hand that surface (or print for `lege`, per
//! `docs/architecture/accessibility.md`'s "everything the shell shows is available through
//! `lege` too"), given one `org.legeos.Resources1.PressureAction` signal plus the context the
//! signal alone does not carry — who is on screen, what time it is, how big the offending group
//! got. No signal-subscription plumbing lives here on purpose: wiring a live
//! `receive_pressure_action` stream is for whichever track ends up owning the overlay surface,
//! and does not change one word this function produces.

use serde::{Deserialize, Serialize};

/// `org.legeos.Resources1.PressureAction`'s `kind` values (`interfaces/dbus/org.legeos.Resources1.xml`,
/// mirrored from `services/lege-resourced/src/policy.rs::Action::kind`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PressureActionKind {
    TrimCaches,
    SetMemoryHigh,
    Reclaim,
    RequestCheckpoint,
    KillCgroup,
    RestoreLimits,
}

impl PressureActionKind {
    /// Parse the wire string; `None` for anything not in the fixed set above.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "trim-caches" => Self::TrimCaches,
            "set-memory-high" => Self::SetMemoryHigh,
            "reclaim" => Self::Reclaim,
            "request-checkpoint" => Self::RequestCheckpoint,
            "kill-cgroup" => Self::KillCgroup,
            "restore-limits" => Self::RestoreLimits,
            _ => return None,
        })
    }
}

/// One `PressureAction` signal (`kind`, `instance_id`, `applied`, `reason`) plus the enrichment
/// the shell has on hand but the signal does not carry: a display name for the offender, when it
/// happened, and — when the caller has them — the figures behind `reason`'s prose. Every field
/// here is plain data so [`render`] stays a pure function, testable with literal values and no
/// live bus at all.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PressureEvent {
    pub kind: PressureActionKind,
    pub instance_id: String,
    pub applied: bool,
    /// The signal's own `reason` — already meant to be human-readable
    /// (`org.legeos.Resources1.xml`: "the sentence the history event and the pressure overlay
    /// show the user"). Used verbatim as the fallback clause when `memory_figure`/`symptom`
    /// below are not available, so this function never has to invent a cause it was not given.
    pub reason: String,
    /// Display name for the offender: the application's human name if the shell has one, else
    /// its canonical `AppId`, else the bare instance id — resolving that is the caller's job.
    pub offender: String,
    /// Preformatted wall-clock time, e.g. `"14:32"` — the caller already knows it and formatting
    /// one is not this function's concern.
    pub time_label: String,
    /// e.g. `"13.4 GiB"`, if the caller has the figure behind `reason`.
    pub memory_figure: Option<String>,
    /// e.g. `"sustained full memory stalls"`, if the caller has the symptom behind `reason`.
    pub symptom: Option<String>,
    /// Whether a checkpoint captured session state before this action — what makes "Restore?"
    /// an honest question to ask afterwards rather than an empty promise.
    pub session_saved: bool,
}

/// One of `lege_recovery_v1`'s four `choice` values, labelled for the overlay text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryChoice {
    Wait,
    CheckpointAndTerminate,
    Terminate,
    OpenTerminal,
}

impl RecoveryChoice {
    pub const ALL: [RecoveryChoice; 4] = [
        Self::Wait,
        Self::CheckpointAndTerminate,
        Self::Terminate,
        Self::OpenTerminal,
    ];

    /// The clause this choice contributes to the closing question of a still-in-progress
    /// overlay (`render`'s live path).
    fn clause(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::CheckpointAndTerminate => "save its state and end it",
            Self::Terminate => "end it now, unsaved",
            Self::OpenTerminal => "open a terminal",
        }
    }
}

/// The trigger clause: what crossed a limit, in words. Falls back to the signal's own `reason`
/// when the caller could not supply the structured figures, so the sentence is always complete
/// even when only the wire fields are known.
fn trigger_clause(event: &PressureEvent) -> String {
    match (&event.memory_figure, &event.symptom) {
        (Some(memory), Some(symptom)) => {
            format!("its application group reached {memory} and caused {symptom}")
        }
        (Some(memory), None) => format!("its application group reached {memory}"),
        (None, Some(symptom)) => format!("it caused {symptom}"),
        (None, None) => event.reason.clone(),
    }
}

/// Render one [`PressureEvent`] as the sentence the pressure overlay (and `lege`) shows.
///
/// Two shapes, chosen by `kind`/`applied`:
/// * `KillCgroup` with `applied` true — the offender is already gone, so there is nothing left
///   to wait on or terminate; the question is whether to restore the saved session (the target
///   sentence from the architecture doc).
/// * everything else — the offender may still be salvageable, so the sentence ends with the
///   four `lege_recovery_v1` choices instead.
pub fn render(event: &PressureEvent) -> String {
    match event.kind {
        PressureActionKind::KillCgroup if event.applied => {
            let session_clause = if event.session_saved {
                "Session state was saved."
            } else {
                "Unsaved work may have been lost."
            };
            format!(
                "{offender} was terminated at {time} after {trigger}. {session_clause} Restore?",
                offender = event.offender,
                time = event.time_label,
                trigger = trigger_clause(event),
            )
        }
        PressureActionKind::RestoreLimits if event.applied => format!(
            "{offender}: pressure eased at {time}; limits restored to normal.",
            offender = event.offender,
            time = event.time_label,
        ),
        PressureActionKind::RequestCheckpoint | PressureActionKind::KillCgroup => {
            let choices = oxford_join(
                &RecoveryChoice::ALL
                    .iter()
                    .map(|choice| choice.clause())
                    .collect::<Vec<_>>(),
            );
            format!(
                "{offender} is under memory pressure at {time}: {trigger}. {choices_capitalized}?",
                offender = event.offender,
                time = event.time_label,
                trigger = trigger_clause(event),
                choices_capitalized = capitalize_first(&choices),
            )
        }
        _ => format!(
            "{offender}: {reason}",
            offender = event.offender,
            reason = event.reason
        ),
    }
}

/// What a `PressureChanged` level means, in one sentence a person can act on.
///
/// The wire value is a bare word (`normal|elevated|severe|critical`, from
/// `services/lege-resourced/src/policy.rs::PressureLevel`) and a bare word is not actionable —
/// "severe" does not tell anyone whether to close a tab or save their work. Both the overlay and
/// Settings' resources page need the same sentence, so it lives here rather than being written
/// twice in two registers.
pub fn describe_level(level: &str) -> &'static str {
    match level {
        "normal" => "Memory is comfortable; nothing is being contained.",
        "elevated" => {
            "Memory is tight. Background applications are being asked to drop caches; nothing has \
             been stopped."
        }
        "severe" => {
            "Memory is short. The largest non-foreground application is being held back and \
             reclaimed from; save anything you care about."
        }
        "critical" => {
            "Memory is nearly exhausted. An application will be asked to save and may be stopped \
             shortly to keep the session responsive."
        }
        _ => "Unrecognised pressure level.",
    }
}

/// `["wait", "end it", "open a terminal"]` → `"wait, end it, or open a terminal"`.
fn oxford_join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} or {second}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
    }
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_event() -> PressureEvent {
        PressureEvent {
            kind: PressureActionKind::KillCgroup,
            instance_id: "3f9c1e0a-1111-4a2b-9c3d-000000000001".to_owned(),
            applied: true,
            reason: "Waterfox terminated: persistent critical memory pressure; usage=13721 MiB"
                .to_owned(),
            offender: "Waterfox".to_owned(),
            time_label: "14:32".to_owned(),
            memory_figure: Some("13.4 GiB".to_owned()),
            symptom: Some("sustained full memory stalls".to_owned()),
            session_saved: true,
        }
    }

    #[test]
    fn kill_cgroup_matches_the_architecture_docs_worked_example() {
        let event = base_event();
        assert_eq!(
            render(&event),
            "Waterfox was terminated at 14:32 after its application group reached 13.4 GiB and \
             caused sustained full memory stalls. Session state was saved. Restore?"
        );
    }

    #[test]
    fn an_unsaved_termination_says_so_instead_of_offering_restore_confidently() {
        let mut event = base_event();
        event.session_saved = false;
        assert_eq!(
            render(&event),
            "Waterfox was terminated at 14:32 after its application group reached 13.4 GiB and \
             caused sustained full memory stalls. Unsaved work may have been lost. Restore?"
        );
    }

    #[test]
    fn missing_figures_fall_back_to_the_signals_own_reason() {
        let mut event = base_event();
        event.memory_figure = None;
        event.symptom = None;
        assert_eq!(
            render(&event),
            "Waterfox was terminated at 14:32 after Waterfox terminated: persistent critical \
             memory pressure; usage=13721 MiB. Session state was saved. Restore?"
        );
    }

    #[test]
    fn a_still_recoverable_offender_is_offered_the_four_wire_choices() {
        let mut event = base_event();
        event.kind = PressureActionKind::RequestCheckpoint;
        let text = render(&event);
        assert_eq!(
            text,
            "Waterfox is under memory pressure at 14:32: its application group reached 13.4 GiB \
             and caused sustained full memory stalls. Wait, save its state and end it, end it \
             now, unsaved, or open a terminal?"
        );
    }

    #[test]
    fn a_decision_not_yet_performed_falls_back_to_a_plain_reason_line() {
        let mut event = base_event();
        event.kind = PressureActionKind::SetMemoryHigh;
        event.applied = false;
        event.reason =
            "Waterfox throttled to 4096 MiB (memory.high) under memory pressure".to_owned();
        assert_eq!(
            render(&event),
            "Waterfox: Waterfox throttled to 4096 MiB (memory.high) under memory pressure"
        );
    }

    #[test]
    fn restored_limits_reads_as_all_clear() {
        let mut event = base_event();
        event.kind = PressureActionKind::RestoreLimits;
        event.applied = true;
        assert_eq!(
            render(&event),
            "Waterfox: pressure eased at 14:32; limits restored to normal."
        );
    }

    #[test]
    fn every_pressure_level_has_an_actionable_sentence() {
        for level in ["normal", "elevated", "severe", "critical"] {
            let sentence = describe_level(level);
            assert!(
                sentence.ends_with('.'),
                "{level} is not a complete sentence: {sentence}"
            );
            assert_ne!(sentence, "Unrecognised pressure level.");
        }
        assert!(describe_level("meltdown").starts_with("Unrecognised"));
    }

    #[test]
    fn the_worrying_levels_tell_the_user_what_to_do() {
        assert!(describe_level("severe").contains("save"));
        assert!(describe_level("critical").contains("save"));
        assert!(describe_level("normal").contains("nothing is being contained"));
    }

    #[test]
    fn every_wire_kind_string_parses() {
        for (wire, kind) in [
            ("trim-caches", PressureActionKind::TrimCaches),
            ("set-memory-high", PressureActionKind::SetMemoryHigh),
            ("reclaim", PressureActionKind::Reclaim),
            ("request-checkpoint", PressureActionKind::RequestCheckpoint),
            ("kill-cgroup", PressureActionKind::KillCgroup),
            ("restore-limits", PressureActionKind::RestoreLimits),
        ] {
            assert_eq!(PressureActionKind::parse(wire), Some(kind));
        }
        assert_eq!(PressureActionKind::parse("evict-tab"), None);
    }
}
