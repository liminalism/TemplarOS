//! Pre-activation gates: what has to be true before a daemon may be restarted under a running
//! session.
//!
//! Installing a set is safe — apt takes it or leaves it. Restarting things under a session that
//! is already running is where a deployment can do damage, and the damage is not always visible
//! at the moment it happens. Three questions are asked first, and every one of them is asked of
//! *data the set carries*, not of a source checkout: the machine being deployed to may not have
//! one, and a gate that only works on the developer's own laptop is not a gate.
//!
//! 1. **Did a shipped D-Bus signature change?** If it did, a restarted daemon will speak
//!    something the shell and applications already running were not built against. They will not
//!    crash; they will start returning errors from calls that used to work, which is the failure
//!    mode that reads as an application bug for a week. So the whole restartable set is held for
//!    a new session, where everything comes round together.
//! 2. **Would this break the one-generation rollback rule?** A set whose stores need a reader
//!    newer than the *previous* set's binaries cannot be rolled back — the old binaries would
//!    refuse the migrated database. That is allowed, but only deliberately, and only with a
//!    snapshot taken first.
//! 3. **Is hostd in the middle of a transaction?** A staged kernel or driver update outlives the
//!    command that started it. Restarting the daemon mid-flight interrupts something a person is
//!    waiting on, so hostd is left alone and picks up the new build when it finishes.

use super::{
    activate::Conditions,
    repo::{Repo, SetManifest},
};
use anyhow::{Result, bail};
use lege_build::{
    BuildSet, StoreSchema,
    components::{Activation, Matrix},
    interfaces::{Change, diff},
};
use std::{collections::BTreeMap, path::Path};

/// What the gates decided.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GateReport {
    /// The conditions to hand to the activation planner.
    pub conditions: Conditions,
    /// One line per gate, for the deployment report.
    pub notes: Vec<String>,
}

/// Runs every gate.
///
/// `previous` is the set currently deployed, if there is one; the first deployment on a machine
/// has nothing to compare against and passes every gate, which is correct — there are no clients
/// from an earlier set to protect.
///
/// # Errors
/// Only for the schema gate, which *refuses* rather than deferring: the others turn into
/// conditions the activation planner acts on.
pub fn run(
    matrix: &Matrix,
    next: &BuildSet,
    previous: Option<&SetManifest>,
    hostd_busy: bool,
    snapshot_taken: bool,
) -> Result<GateReport> {
    let mut report = GateReport::default();
    report.conditions.hostd_busy = hostd_busy;
    if hostd_busy {
        report
            .notes
            .push("hostd has a transaction in flight; it is left running".to_owned());
    }

    schema_gate(next, previous, snapshot_taken)?;
    report.notes.push(schema_note(next, previous));

    let breaking = interface_changes(next, previous);
    if breaking.is_empty() {
        report
            .notes
            .push("interface signatures compatible ✓".to_owned());
    } else {
        let reason = format!(
            "an interface signature changed in this set ({}); the session has to come round \
             together",
            breaking.first().cloned().unwrap_or_default()
        );
        for component in matrix
            .components
            .iter()
            .filter(|component| component.activation == Activation::Restart)
        {
            report
                .conditions
                .blocked
                .insert(component.name.clone(), reason.clone());
        }
        report.notes.push(format!(
            "{} shipped signature(s) changed; nothing is restarted in place",
            breaking.len()
        ));
    }
    Ok(report)
}

/// Every breaking interface change between the deployed set and the one being deployed.
///
/// A set published before signatures were recorded carries none, and comparing against nothing
/// would report every interface as removed. That is the wrong answer twice over — it is not true,
/// and it would block every deployment on such a machine forever — so an absent record means "no
/// judgement available", and the gate passes.
#[must_use]
pub fn interface_changes(next: &BuildSet, previous: Option<&SetManifest>) -> Vec<String> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    if previous.interfaces.is_empty() || next.interfaces.is_empty() {
        return Vec::new();
    }
    diff(&previous.interfaces, &next.interfaces)
        .iter()
        .filter(|change| change.is_breaking())
        .map(Change::describe)
        .collect()
}

/// Refuse a set that cannot be rolled back, unless that was asked for.
fn schema_gate(
    next: &BuildSet,
    previous: Option<&SetManifest>,
    snapshot_taken: bool,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    for (store, breaking) in unrollbackable_stores(&next.schemas, &previous.schemas) {
        if snapshot_taken {
            continue;
        }
        bail!(
            "this set migrates the {store} store to a form the set you are running ({}) cannot \
             read: it needs a reader at schema version {}, and that set writes {}. Deploying it \
             would make `lege dev rollback` impossible. Pass --snapshot-state to copy the store \
             first, and the rollback becomes: reinstall the previous set and restore that copy.",
            previous.build_set,
            breaking.needs,
            breaking.previous_version,
        );
    }
    Ok(())
}

/// A store whose new reader floor is above what the previous set writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaBreak {
    pub needs: u32,
    pub previous_version: u32,
}

/// Every store this set puts out of the previous set's reach.
#[must_use]
pub fn unrollbackable_stores(
    next: &BTreeMap<String, StoreSchema>,
    previous: &BTreeMap<String, StoreSchema>,
) -> Vec<(String, SchemaBreak)> {
    next.iter()
        .filter_map(|(store, schema)| {
            let was = previous.get(store)?;
            (schema.min_reader_version > was.version).then_some((
                store.clone(),
                SchemaBreak {
                    needs: schema.min_reader_version,
                    previous_version: was.version,
                },
            ))
        })
        .collect()
}

fn schema_note(next: &BuildSet, previous: Option<&SetManifest>) -> String {
    let Some(previous) = previous else {
        return "schema compatible ✓ (nothing deployed here before)".to_owned();
    };
    if next.schemas.is_empty() || previous.schemas.is_empty() {
        return "schema compatibility unknown (an older set recorded none)".to_owned();
    }
    let stores: Vec<String> = next
        .schemas
        .iter()
        .map(|(store, schema)| format!("{store} v{}", schema.version))
        .collect();
    format!("schema compatible ✓ ({})", stores.join(", "))
}

/// Whether `lege-hostd` has a transaction that has not reached a terminal phase.
///
/// Read from hostd's own journal directory rather than asked over the system bus: this runs as
/// root during a deployment, the journal is fsynced before each phase change precisely so it can
/// be trusted after a crash, and a bus round-trip would make a *busy* hostd — the case this is
/// about — the hardest one to get an answer from.
#[must_use]
pub fn hostd_busy(root: &Path) -> bool {
    // Kept as a list rather than a negation so a new phase is non-terminal until someone decides
    // otherwise; the failure of guessing wrong here is interrupting a kernel update.
    const TERMINAL: [&str; 3] = ["promoted", "rolled-back", "failed"];

    let dir = root.join("var/lib/legeos/hostd/transactions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            return false;
        }
        let Ok(body) = std::fs::read(&path) else {
            return false;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return false;
        };
        value["phase"]
            .as_str()
            .is_some_and(|phase| !TERMINAL.contains(&phase))
    })
}

/// Snapshots every store this set would put out of the previous set's reach.
///
/// Returns the paths written. Taking the snapshot is what makes a floor-raising migration
/// survivable, so it happens *before* the install rather than after.
///
/// # Errors
/// If a store cannot be copied.
pub fn snapshot_stores(root: &Path, build_set: &str) -> Result<Vec<std::path::PathBuf>> {
    let source = root.join(
        lege_state::default_path()
            .strip_prefix("/")
            .unwrap_or(&lege_state::default_path()),
    );
    if !source.exists() {
        return Ok(Vec::new());
    }
    let target = root
        .join("var/lib/legeos/snapshots")
        .join(format!("session-before-{build_set}.db"));
    let connection = rusqlite::Connection::open(&source)?;
    lege_state::snapshot_to(&connection, &target)?;
    Ok(vec![target])
}

/// Where the archive's currently deployed set is described, if it has one.
///
/// # Errors
/// If the archive cannot be read.
pub fn deployed_set(repo: &Repo) -> Result<Option<SetManifest>> {
    let Some(current) = repo.current()? else {
        return Ok(None);
    };
    Ok(repo.manifest(&current).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_build::interfaces::InterfaceSignatures;

    fn signatures(status: &str) -> BTreeMap<String, InterfaceSignatures> {
        let mut methods = BTreeMap::new();
        methods.insert("Status".to_owned(), status.to_owned());
        [(
            "org.legeos.Resources1".to_owned(),
            InterfaceSignatures {
                methods,
                ..InterfaceSignatures::default()
            },
        )]
        .into_iter()
        .collect()
    }

    fn schemas(version: u32, floor: u32) -> BTreeMap<String, StoreSchema> {
        [(
            "session".to_owned(),
            StoreSchema {
                version,
                min_reader_version: floor,
            },
        )]
        .into_iter()
        .collect()
    }

    fn build_set(id: &str) -> BuildSet {
        BuildSet {
            id: id.to_owned(),
            git_sha: "aaaaaaa".to_owned(),
            dirty: false,
            epoch: 1,
            packages: Vec::new(),
            interfaces: signatures("()->(sb)"),
            schemas: schemas(7, 0),
        }
    }

    fn deployed(id: &str) -> SetManifest {
        SetManifest {
            build_set: id.to_owned(),
            published_at_ms: 0,
            packages: Vec::new(),
            interfaces: signatures("()->(sb)"),
            schemas: schemas(7, 0),
        }
    }

    #[test]
    fn an_unchanged_set_restarts_normally() {
        let report = run(
            &Matrix::embedded(),
            &build_set("new"),
            Some(&deployed("old")),
            false,
            false,
        )
        .unwrap();
        assert!(report.conditions.blocked.is_empty(), "{report:#?}");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("interface signatures compatible")),
            "{report:#?}"
        );
    }

    #[test]
    fn a_changed_signature_holds_the_whole_restartable_set_for_a_new_session() {
        // The failure this gate exists for is not a crash: a restarted daemon speaking a changed
        // signature makes calls that used to work start returning errors, which reads as an
        // application bug for about a week.
        let mut next = build_set("new");
        next.interfaces = signatures("()->(sbu)");
        let report = run(
            &Matrix::embedded(),
            &next,
            Some(&deployed("old")),
            false,
            false,
        )
        .unwrap();
        let matrix = Matrix::embedded();
        for component in &matrix.components {
            if component.activation == Activation::Restart {
                assert!(
                    report.conditions.blocked.contains_key(&component.name),
                    "{} must not be restarted in place",
                    component.name
                );
            }
        }
        assert!(
            report.conditions.blocked["shell"].contains("come round together"),
            "{:?}",
            report.conditions.blocked["shell"]
        );
    }

    #[test]
    fn a_new_member_is_not_a_reason_to_hold_the_session() {
        let mut next = build_set("new");
        next.interfaces
            .get_mut("org.legeos.Resources1")
            .unwrap()
            .methods
            .insert("Explain".to_owned(), "(s)->(s)".to_owned());
        let report = run(
            &Matrix::embedded(),
            &next,
            Some(&deployed("old")),
            false,
            false,
        )
        .unwrap();
        assert!(report.conditions.blocked.is_empty(), "{report:#?}");
    }

    #[test]
    fn a_set_that_cannot_be_rolled_back_is_refused_and_says_how_to_proceed() {
        let mut next = build_set("new");
        next.schemas = schemas(9, 9);
        let error = run(
            &Matrix::embedded(),
            &next,
            Some(&deployed("old")),
            false,
            false,
        )
        .unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("session store"), "{text}");
        assert!(text.contains("--snapshot-state"), "{text}");
        assert!(text.contains("rollback"), "{text}");
    }

    #[test]
    fn a_snapshot_makes_the_unrollbackable_set_deployable() {
        let mut next = build_set("new");
        next.schemas = schemas(9, 9);
        let report = run(
            &Matrix::embedded(),
            &next,
            Some(&deployed("old")),
            false,
            true,
        )
        .unwrap();
        assert!(report.conditions.blocked.is_empty());
    }

    #[test]
    fn a_floor_the_previous_set_still_reaches_is_not_a_break() {
        let mut next = build_set("new");
        next.schemas = schemas(8, 7);
        assert!(
            unrollbackable_stores(&next.schemas, &deployed("old").schemas).is_empty(),
            "a floor of 7 against a previous set writing 7 is exactly one generation"
        );
    }

    #[test]
    fn the_first_deployment_on_a_machine_passes_every_gate() {
        let report = run(&Matrix::embedded(), &build_set("new"), None, false, false).unwrap();
        assert!(report.conditions.blocked.is_empty());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("nothing deployed here before"))
        );
    }

    #[test]
    fn a_set_published_before_signatures_were_recorded_is_not_read_as_a_removal() {
        // Comparing against an empty record would report every interface as removed, which is
        // both untrue and would block every deployment on such a machine for ever.
        let mut previous = deployed("old");
        previous.interfaces.clear();
        assert!(interface_changes(&build_set("new"), Some(&previous)).is_empty());
    }

    #[test]
    fn a_busy_hostd_is_reported_and_left_alone() {
        let report = run(
            &Matrix::embedded(),
            &build_set("new"),
            Some(&deployed("old")),
            true,
            false,
        )
        .unwrap();
        assert!(report.conditions.hostd_busy);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("transaction in flight"))
        );
    }

    #[test]
    fn hostd_is_busy_only_while_a_transaction_is_not_terminal() {
        let dir = std::env::temp_dir().join(format!("lege-hostd-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let transactions = dir.join("var/lib/legeos/hostd/transactions");
        std::fs::create_dir_all(&transactions).unwrap();

        assert!(!hostd_busy(&dir), "an empty journal is not busy");
        std::fs::write(
            transactions.join("a.json"),
            br#"{"id":"a","phase":"promoted"}"#,
        )
        .unwrap();
        assert!(!hostd_busy(&dir), "a finished transaction is not busy");
        std::fs::write(
            transactions.join("b.json"),
            br#"{"id":"b","phase":"awaiting-reboot"}"#,
        )
        .unwrap();
        assert!(hostd_busy(&dir), "a staged update is busy");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
