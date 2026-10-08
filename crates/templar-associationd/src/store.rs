//! Persistence for the association database, plus the combined transactions ADR-0001 asks for:
//! "change the default PDF handler *and* record the history entry" is one commit. Every mutating
//! function here writes its own row(s), a `change_log` entry (so `History1.Undo` can reverse it)
//! and the general `history` event in the same [`templar_state::Store::transaction`] call.

use crate::engine::{AssociationDb, ExtensionRule};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use templar_session_core::{
    change_log::{self, ChangeLogEntry},
    history::{Event, EventAction, EventId, EventKind, append_within},
};
use templar_state::{Store, now_ms};
use templar_types::AppId;

/// Internal marker key inside the generic `settings_values` table: has `mimeapps.list` been
/// imported yet? Not part of the typed settings schema, so it never appears in `Settings1`.
const MIMEAPPS_IMPORTED_KEY: &str = "_internal.associations.mimeapps-imported";

/// Load the whole association database.
pub fn load_db(store: &Store) -> Result<AssociationDb> {
    let connection = store.lock();
    load_db_within(&connection)
}

/// Read a staged association view without reacquiring the Store lock.
pub fn load_db_within(connection: &rusqlite::Connection) -> Result<AssociationDb> {
    let mut db = AssociationDb::default();

    let mut mime = connection
        .prepare("SELECT mime, app_id FROM mime_defaults")
        .context("preparing mime_defaults query")?;
    for row in mime.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (mime, app_id) = row?;
        db.mime_defaults.insert(mime, parse_app_id(&app_id)?);
    }
    drop(mime);

    let mut extensions = connection
        .prepare("SELECT extension, app_id, override_specific_mime FROM extension_defaults")
        .context("preparing extension_defaults query")?;
    for row in extensions.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, bool>(2)?,
        ))
    })? {
        let (extension, app_id, override_specific_mime) = row?;
        db.extension_defaults.insert(
            extension,
            ExtensionRule {
                app_id: parse_app_id(&app_id)?,
                override_specific_mime,
            },
        );
    }
    drop(extensions);

    let mut projects = connection
        .prepare("SELECT project_id, mime, app_id FROM project_mime_defaults")
        .context("preparing project_mime_defaults query")?;
    for row in projects.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (project_id, mime, app_id) = row?;
        db.project_mime_defaults
            .entry(project_id)
            .or_default()
            .insert(mime, parse_app_id(&app_id)?);
    }
    Ok(db)
}

fn parse_app_id(value: &str) -> Result<AppId> {
    value
        .parse()
        .with_context(|| format!("invalid app id stored in associations: {value:?}"))
}

/// Has the one-time `mimeapps.list` import already happened?
pub fn mimeapps_already_imported(store: &Store) -> Result<bool> {
    let connection = store.lock();
    let value: Option<String> = connection
        .query_row(
            "SELECT value_json FROM settings_values WHERE key = ?1",
            params![MIMEAPPS_IMPORTED_KEY],
            |row| row.get(0),
        )
        .optional()
        .context("checking mimeapps import marker")?;
    Ok(value.is_some())
}

/// Mark the one-time import done, and persist every resolved default from it — all in one
/// transaction, as an ordinary bulk write (not through the individual `*_within` helpers below,
/// which each also write a history event; a first-run import is not itself a user decision worth
/// narrating one event per MIME type).
pub fn mark_mimeapps_imported_within(
    tx: &Transaction<'_>,
    resolved: &[(String, AppId)],
) -> Result<()> {
    for (mime, app_id) in resolved {
        tx.execute(
            "INSERT INTO mime_defaults (mime, app_id) VALUES (?1, ?2)
             ON CONFLICT (mime) DO UPDATE SET app_id = excluded.app_id",
            params![mime, app_id.as_str()],
        )
        .context("importing mime default")?;
    }
    tx.execute(
        "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, 'true', ?2)
         ON CONFLICT (key) DO UPDATE SET value_json = 'true', updated_ms = excluded.updated_ms",
        params![MIMEAPPS_IMPORTED_KEY, now_ms()],
    )
    .context("recording mimeapps import marker")?;
    Ok(())
}

fn history_event(app_id: Option<AppId>, message: String, change_id: i64) -> Event {
    Event {
        id: EventId::new(),
        at_ms: now_ms(),
        kind: EventKind::AssociationChange,
        app_id,
        instance_id: None,
        message,
        cause: Some("user".into()),
        actions: vec![EventAction {
            label: "Undo".into(),
            command: change_log::undo_command(change_id),
        }],
        reversible: true,
    }
}

/// Persist `mime -> app_id`, recording history + the undo log in one commit. Returns the new
/// history event's ID (useful for tests and for correlating with `DefaultsChanged`).
pub fn set_mime_default_within(
    tx: &Transaction<'_>,
    mime: &str,
    app_id: &AppId,
    logical_app_id: Option<&templar_types::LogicalAppId>,
) -> Result<EventId> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT app_id FROM mime_defaults WHERE mime = ?1",
            params![mime],
            |row| row.get(0),
        )
        .optional()
        .context("reading previous mime default")?;
    tx.execute(
        // The choice is stored against the variant, always: a person picked *that copy*. The
        // logical id travels alongside as a read model — "which application did this mean" — so a
        // sibling can answer when the chosen copy is uninstalled, without the stored choice ever
        // having been silently widened.
        "INSERT INTO mime_defaults (mime, app_id, logical_app_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (mime) DO UPDATE SET
            app_id = excluded.app_id, logical_app_id = excluded.logical_app_id",
        params![
            mime,
            app_id.as_str(),
            logical_app_id.map(templar_types::LogicalAppId::as_str)
        ],
    )
    .context("persisting mime default")?;

    let event_id = EventId::new();
    let change_id = change_log::record_within(
        tx,
        "associations.mime",
        mime,
        previous.as_deref().map(|v| format!("{v:?}")).as_deref(),
        Some(&format!("{:?}", app_id.as_str())),
        event_id,
    )?;
    let message = match &previous {
        Some(old) => format!("The default application for {mime} changed from {old} to {app_id}."),
        None => format!("The default application for {mime} was set to {app_id}."),
    };
    let mut event = history_event(Some(app_id.clone()), message, change_id);
    event.id = event_id;
    append_within(tx, &event)?;
    Ok(event_id)
}

/// Persist an extension rule, with history/undo (see [`set_mime_default_within`]).
pub fn set_extension_default_within(
    tx: &Transaction<'_>,
    extension: &str,
    rule: &ExtensionRule,
    logical_app_id: Option<&templar_types::LogicalAppId>,
) -> Result<EventId> {
    let previous: Option<(String, bool)> = tx
        .query_row(
            "SELECT app_id, override_specific_mime FROM extension_defaults WHERE extension = ?1",
            params![extension],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("reading previous extension default")?;
    tx.execute(
        "INSERT INTO extension_defaults (extension, app_id, override_specific_mime, logical_app_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (extension) DO UPDATE SET
            app_id = excluded.app_id, override_specific_mime = excluded.override_specific_mime,
            logical_app_id = excluded.logical_app_id",
        params![
            extension,
            rule.app_id.as_str(),
            rule.override_specific_mime,
            logical_app_id.map(templar_types::LogicalAppId::as_str)
        ],
    )
    .context("persisting extension default")?;

    let event_id = EventId::new();
    let old_json = previous.as_ref().map(|(app, over)| {
        serde_json::json!({"app_id": app, "override_specific_mime": over}).to_string()
    });
    let new_json =
        serde_json::json!({"app_id": rule.app_id.as_str(), "override_specific_mime": rule.override_specific_mime})
            .to_string();
    let change_id = change_log::record_within(
        tx,
        "associations.extension",
        extension,
        old_json.as_deref(),
        Some(&new_json),
        event_id,
    )?;
    let message = format!(".{extension} files now always open with {}.", rule.app_id);
    let mut event = history_event(Some(rule.app_id.clone()), message, change_id);
    event.id = event_id;
    append_within(tx, &event)?;
    Ok(event_id)
}

/// Persist a project-scoped default, with history/undo.
pub fn set_project_mime_default_within(
    tx: &Transaction<'_>,
    project_id: &str,
    mime: &str,
    app_id: &AppId,
    logical_app_id: Option<&templar_types::LogicalAppId>,
) -> Result<EventId> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT app_id FROM project_mime_defaults WHERE project_id = ?1 AND mime = ?2",
            params![project_id, mime],
            |row| row.get(0),
        )
        .optional()
        .context("reading previous project mime default")?;
    tx.execute(
        "INSERT INTO project_mime_defaults (project_id, mime, app_id, logical_app_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (project_id, mime) DO UPDATE SET
            app_id = excluded.app_id, logical_app_id = excluded.logical_app_id",
        params![
            project_id,
            mime,
            app_id.as_str(),
            logical_app_id.map(templar_types::LogicalAppId::as_str)
        ],
    )
    .context("persisting project mime default")?;

    let key = format!("{project_id}\u{1f}{mime}");
    let event_id = EventId::new();
    let change_id = change_log::record_within(
        tx,
        "associations.project-mime",
        &key,
        previous.as_deref().map(|v| format!("{v:?}")).as_deref(),
        Some(&format!("{:?}", app_id.as_str())),
        event_id,
    )?;
    let message = format!("In project {project_id}, {mime} now opens with {app_id}.");
    let mut event = history_event(Some(app_id.clone()), message, change_id);
    event.id = event_id;
    append_within(tx, &event)?;
    Ok(event_id)
}

/// What kind of rule [`forget_within`] removed (used to pick the SQL and the history sentence).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForgetScope {
    Mime,
    Extension,
    ProjectMime,
}

/// Remove a rule (mime default, extension rule, or one project's mime default), with
/// history/undo. `project_id` is only consulted for [`ForgetScope::ProjectMime`].
pub fn forget_within(
    tx: &Transaction<'_>,
    scope: ForgetScope,
    key: &str,
    project_id: Option<&str>,
) -> Result<Option<EventId>> {
    let (removed_app_id, change_key, old_json, message): (
        Option<String>,
        String,
        Option<String>,
        String,
    ) = match scope {
        ForgetScope::Mime => {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT app_id FROM mime_defaults WHERE mime = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.is_some() {
                tx.execute("DELETE FROM mime_defaults WHERE mime = ?1", params![key])?;
            }
            let message = format!("The default application for {key} was cleared.");
            (
                previous.clone(),
                key.to_owned(),
                previous.map(|v| format!("{v:?}")),
                message,
            )
        }
        ForgetScope::Extension => {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT app_id FROM extension_defaults WHERE extension = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.is_some() {
                tx.execute(
                    "DELETE FROM extension_defaults WHERE extension = ?1",
                    params![key],
                )?;
            }
            let message = format!(".{key} no longer has a forced handler.");
            (
                previous.clone(),
                key.to_owned(),
                previous.map(|v| format!("{v:?}")),
                message,
            )
        }
        ForgetScope::ProjectMime => {
            let project_id =
                project_id.context("ForgetScope::ProjectMime requires a project_id")?;
            let previous: Option<String> = tx
                .query_row(
                    "SELECT app_id FROM project_mime_defaults WHERE project_id = ?1 AND mime = ?2",
                    params![project_id, key],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.is_some() {
                tx.execute(
                    "DELETE FROM project_mime_defaults WHERE project_id = ?1 AND mime = ?2",
                    params![project_id, key],
                )?;
            }
            let message = format!("Project {project_id}'s default for {key} was cleared.");
            (
                previous.clone(),
                format!("{project_id}\u{1f}{key}"),
                previous.map(|v| format!("{v:?}")),
                message,
            )
        }
    };

    let Some(removed_app_id) = removed_app_id else {
        return Ok(None); // nothing to forget; not an error, just a no-op.
    };

    let domain = match scope {
        ForgetScope::Mime => "associations.mime",
        ForgetScope::Extension => "associations.extension",
        ForgetScope::ProjectMime => "associations.project-mime",
    };
    let event_id = EventId::new();
    let change_id =
        change_log::record_within(tx, domain, &change_key, old_json.as_deref(), None, event_id)?;
    let mut event = history_event(removed_app_id.parse().ok(), message, change_id);
    event.id = event_id;
    append_within(tx, &event)?;
    Ok(Some(event_id))
}

/// Apply the removals [`crate::engine::AssociationEngine::prune_stale`] found — an application
/// was uninstalled — recording one history event per removal (no undo: the target application
/// is gone, so there is nothing to restore to).
pub fn apply_prune_within(tx: &Transaction<'_>, removed: &[(String, AppId)]) -> Result<()> {
    for (key, app_id) in removed {
        if let Some(extension) = key.strip_prefix('.') {
            tx.execute(
                "DELETE FROM extension_defaults WHERE extension = ?1",
                params![extension],
            )?;
        } else {
            tx.execute("DELETE FROM mime_defaults WHERE mime = ?1", params![key])?;
        }
        append_within(
            tx,
            &Event {
                id: EventId::new(),
                at_ms: now_ms(),
                kind: EventKind::AssociationChange,
                app_id: Some(app_id.clone()),
                instance_id: None,
                message: format!("{app_id} was uninstalled; its default for {key} was removed."),
                cause: Some("prune-stale".into()),
                actions: Vec::new(),
                reversible: false,
            },
        )?;
    }
    Ok(())
}

/// Reverse one `change_log` entry this module produced (`associations.mime`,
/// `associations.extension` or `associations.project-mime`). Writes the row back (or deletes it,
/// if `old_value` is `None` — the change created something that did not exist before). Does not
/// touch the in-memory engine; the caller reloads it from [`load_db`] afterward, since an undo
/// is rare enough that a full reload is simpler and just as correct as a bespoke inverse.
pub fn revert_within(tx: &Transaction<'_>, entry: &ChangeLogEntry) -> Result<()> {
    match entry.domain.as_str() {
        "associations.mime" => match &entry.old_value {
            Some(old) => {
                let app_id = json_app_id(old)?;
                tx.execute(
                    "INSERT INTO mime_defaults (mime, app_id) VALUES (?1, ?2)
                     ON CONFLICT (mime) DO UPDATE SET app_id = excluded.app_id",
                    params![entry.key, app_id.as_str()],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM mime_defaults WHERE mime = ?1",
                    params![entry.key],
                )?;
            }
        },
        "associations.extension" => match &entry.old_value {
            Some(old) => {
                let value: serde_json::Value =
                    serde_json::from_str(old).context("parsing stored extension rule")?;
                let app_id: AppId = value["app_id"]
                    .as_str()
                    .context("extension rule JSON missing app_id")?
                    .parse()
                    .context("extension rule JSON has an invalid app_id")?;
                let override_specific_mime =
                    value["override_specific_mime"].as_bool().unwrap_or(false);
                tx.execute(
                    "INSERT INTO extension_defaults (extension, app_id, override_specific_mime) VALUES (?1, ?2, ?3)
                     ON CONFLICT (extension) DO UPDATE SET
                        app_id = excluded.app_id, override_specific_mime = excluded.override_specific_mime",
                    params![entry.key, app_id.as_str(), override_specific_mime],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM extension_defaults WHERE extension = ?1",
                    params![entry.key],
                )?;
            }
        },
        "associations.project-mime" => {
            let (project_id, mime) = entry
                .key
                .split_once('\u{1f}')
                .context("malformed associations.project-mime change_log key")?;
            match &entry.old_value {
                Some(old) => {
                    let app_id = json_app_id(old)?;
                    tx.execute(
                        "INSERT INTO project_mime_defaults (project_id, mime, app_id) VALUES (?1, ?2, ?3)
                         ON CONFLICT (project_id, mime) DO UPDATE SET app_id = excluded.app_id",
                        params![project_id, mime, app_id.as_str()],
                    )?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM project_mime_defaults WHERE project_id = ?1 AND mime = ?2",
                        params![project_id, mime],
                    )?;
                }
            }
        }
        other => bail!("templar-associationd cannot revert change_log domain {other:?}"),
    }
    Ok(())
}

fn json_app_id(json: &str) -> Result<AppId> {
    let raw: String = serde_json::from_str(json).context("parsing stored app id")?;
    raw.parse().context("stored app id is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_mime_default_persists_history_and_undo_log_together() {
        let store = Store::in_memory().unwrap();
        let app_id: AppId = "org.templaros.Viewer".parse().unwrap();
        let event_id = store
            .transaction(|tx| set_mime_default_within(tx, "application/pdf", &app_id, None))
            .unwrap();

        let db = load_db(&store).unwrap();
        assert_eq!(db.mime_defaults.get("application/pdf"), Some(&app_id));

        let connection = store.lock();
        let message: String = connection
            .query_row(
                "SELECT message FROM history WHERE event_id = ?1",
                params![event_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(message.contains("was set to org.templaros.Viewer"));
        let undone: i64 = connection
            .query_row(
                "SELECT undone FROM change_log WHERE event_id = ?1",
                params![event_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(undone, 0);
    }

    #[test]
    fn forget_is_a_no_op_when_nothing_is_set() {
        let store = Store::in_memory().unwrap();
        let result = store
            .transaction(|tx| forget_within(tx, ForgetScope::Mime, "application/pdf", None))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn forget_removes_the_row_and_records_an_undoable_event() {
        let store = Store::in_memory().unwrap();
        let app_id: AppId = "org.templaros.Viewer".parse().unwrap();
        store
            .transaction(|tx| {
                set_mime_default_within(tx, "application/pdf", &app_id, None).map(|_| ())
            })
            .unwrap();

        let event_id = store
            .transaction(|tx| forget_within(tx, ForgetScope::Mime, "application/pdf", None))
            .unwrap()
            .expect("something was removed");

        let db = load_db(&store).unwrap();
        assert!(db.mime_defaults.is_empty());

        let connection = store.lock();
        let old_value: Option<String> = connection
            .query_row(
                "SELECT old_value FROM change_log WHERE event_id = ?1",
                params![event_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            old_value.is_some(),
            "the removed value must still be recoverable for undo"
        );
    }

    #[test]
    fn mimeapps_import_marker_round_trips() {
        let store = Store::in_memory().unwrap();
        assert!(!mimeapps_already_imported(&store).unwrap());
        let app_id: AppId = "org.templaros.Viewer".parse().unwrap();
        store
            .transaction(|tx| {
                mark_mimeapps_imported_within(tx, &[("application/pdf".into(), app_id.clone())])
            })
            .unwrap();
        assert!(mimeapps_already_imported(&store).unwrap());
        assert_eq!(
            load_db(&store)
                .unwrap()
                .mime_defaults
                .get("application/pdf"),
            Some(&app_id)
        );
    }

    #[test]
    fn revert_within_restores_a_removed_default_and_removes_a_created_one() {
        let store = Store::in_memory().unwrap();
        let viewer: AppId = "org.templaros.Viewer".parse().unwrap();
        let browser: AppId = "org.example.Browser".parse().unwrap();

        // Undo a change: the change_log entry from a Viewer -> Browser switch restores Viewer.
        store
            .transaction(|tx| {
                set_mime_default_within(tx, "application/pdf", &viewer, None).map(|_| ())
            })
            .unwrap();
        let event_id = store
            .transaction(|tx| set_mime_default_within(tx, "application/pdf", &browser, None))
            .unwrap();
        store
            .transaction(|tx| {
                let change_id = templar_session_core::change_log::parse_undo_command(
                    &templar_session_core::history::find_within(tx, event_id)?
                        .unwrap()
                        .actions[0]
                        .command,
                )
                .unwrap();
                let entry = change_log::find_within(tx, change_id)?.unwrap();
                revert_within(tx, &entry)
            })
            .unwrap();
        assert_eq!(
            load_db(&store)
                .unwrap()
                .mime_defaults
                .get("application/pdf"),
            Some(&viewer)
        );

        // Undo a creation (no previous value): the row is removed entirely.
        store
            .transaction(|tx| {
                forget_within(tx, ForgetScope::Mime, "application/pdf", None).map(|_| ())
            })
            .unwrap();
        let create_event = store
            .transaction(|tx| set_mime_default_within(tx, "application/pdf", &viewer, None))
            .unwrap();
        store
            .transaction(|tx| {
                let change_id = templar_session_core::change_log::parse_undo_command(
                    &templar_session_core::history::find_within(tx, create_event)?
                        .unwrap()
                        .actions[0]
                        .command,
                )
                .unwrap();
                let entry = change_log::find_within(tx, change_id)?.unwrap();
                assert_eq!(entry.old_value, None);
                revert_within(tx, &entry)
            })
            .unwrap();
        assert!(load_db(&store).unwrap().mime_defaults.is_empty());
    }
}
