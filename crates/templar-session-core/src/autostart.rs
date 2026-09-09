//! Permissioned autostart. An installed `.desktop` autostart file is a *request*; the session
//! daemon launches only entries that are approved and, for exclusive roles, selected.
//! The TemplarOS session masks `xdg-desktop-autostart.target` so nothing bypasses this.

use crate::{
    change_log,
    history::{Event, EventAction, EventId, EventKind, append_within},
    providers::ProviderRegistry,
};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use templar_state::Store;
use templar_types::{AppId, ProviderRole};

/// Background capabilities an application may request (planning doc 01 §7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundPermission {
    LoginAutostart,
    ContinuousBackground,
    NetworkWhileHidden,
    GlobalShortcuts,
    ScreenCapture,
    InputMonitoring,
    Notifications,
    RemovableMedia,
}

impl BackgroundPermission {
    /// The kebab-case name used in manifests, autostart `X-TemplarOS-Background` entries and the
    /// settings UI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoginAutostart => "login-autostart",
            Self::ContinuousBackground => "continuous-background",
            Self::NetworkWhileHidden => "network-while-hidden",
            Self::GlobalShortcuts => "global-shortcuts",
            Self::ScreenCapture => "screen-capture",
            Self::InputMonitoring => "input-monitoring",
            Self::Notifications => "notifications",
            Self::RemovableMedia => "removable-media",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "login-autostart" => Self::LoginAutostart,
            "continuous-background" => Self::ContinuousBackground,
            "network-while-hidden" => Self::NetworkWhileHidden,
            "global-shortcuts" => Self::GlobalShortcuts,
            "screen-capture" => Self::ScreenCapture,
            "input-monitoring" => Self::InputMonitoring,
            "notifications" => Self::Notifications,
            "removable-media" => Self::RemovableMedia,
            _ => return None,
        })
    }
}

/// One autostart request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartEntry {
    pub app_id: AppId,
    /// If set, the entry only starts when this app is the selected provider for the role.
    pub role: Option<ProviderRole>,
    pub requested: Vec<BackgroundPermission>,
}

/// User/distribution decision for an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    Approved,
    Denied,
    /// Not yet decided: distribution defaults apply (approved for role providers, denied otherwise).
    Undecided,
}

impl Approval {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Undecided => "undecided",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "approved" => Self::Approved,
            "denied" => Self::Denied,
            "undecided" => Self::Undecided,
            _ => return None,
        })
    }
}

/// Outcome per entry, always with a reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Verdict {
    Launch,
    SkippedDenied,
    SkippedNotSelectedProvider { role: ProviderRole },
    SkippedUndecided,
}

/// Decide which autostart entries launch this login.
pub fn plan(
    entries: &[(AutostartEntry, Approval)],
    providers: &ProviderRegistry,
) -> Vec<(AppId, Verdict)> {
    entries
        .iter()
        .map(|(entry, approval)| {
            let verdict = match (approval, entry.role) {
                (Approval::Denied, _) => Verdict::SkippedDenied,
                (_, Some(role)) if !providers.may_autostart(&entry.app_id, role) => {
                    Verdict::SkippedNotSelectedProvider { role }
                }
                (Approval::Approved, _) | (Approval::Undecided, Some(_)) => Verdict::Launch,
                (Approval::Undecided, None) => Verdict::SkippedUndecided,
            };
            (entry.app_id.clone(), verdict)
        })
        .collect()
}

/// Load every recorded approval/denial. An application with no row is [`Approval::Undecided`].
pub fn load_approvals(store: &Store) -> Result<BTreeMap<AppId, Approval>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT app_id, approval FROM autostart_approvals")
        .context("preparing autostart_approvals query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("reading autostart_approvals")?;
    let mut approvals = BTreeMap::new();
    for row in rows {
        let (app_id, approval) = row?;
        let app_id: AppId = app_id
            .parse()
            .with_context(|| format!("invalid app id in autostart_approvals: {app_id:?}"))?;
        let approval = Approval::parse(&approval)
            .with_context(|| format!("invalid approval value: {approval:?}"))?;
        approvals.insert(app_id, approval);
    }
    Ok(approvals)
}

/// Read one application's approval, defaulting to [`Approval::Undecided`].
pub fn approval_for(approvals: &BTreeMap<AppId, Approval>, app_id: &AppId) -> Approval {
    approvals
        .get(app_id)
        .copied()
        .unwrap_or(Approval::Undecided)
}

/// Persist a user/distribution decision. Storing [`Approval::Undecided`] deletes the row (the
/// absence of a row *is* undecided), keeping the table free of no-op entries.
pub fn set_approval_within(tx: &Transaction<'_>, app_id: &AppId, approval: Approval) -> Result<()> {
    if approval == Approval::Undecided {
        tx.execute(
            "DELETE FROM autostart_approvals WHERE app_id = ?1",
            params![app_id.as_str()],
        )
        .context("clearing autostart approval")?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO autostart_approvals (app_id, approval) VALUES (?1, ?2)
         ON CONFLICT (app_id) DO UPDATE SET approval = excluded.approval",
        params![app_id.as_str(), approval.as_str()],
    )
    .context("persisting autostart approval")?;
    Ok(())
}

fn current_approval_within(tx: &Transaction<'_>, app_id: &AppId) -> Result<Option<Approval>> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT approval FROM autostart_approvals WHERE app_id = ?1",
            params![app_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .context("reading current autostart approval")?;
    stored
        .map(|value| {
            Approval::parse(&value).with_context(|| format!("invalid stored approval: {value:?}"))
        })
        .transpose()
}

/// Set an application's background-permission approval, recording history/undo in the same
/// transaction (`org.templaros.Background1.Grant`/`Revoke`; `History1.Undo`'s `background.approval`
/// domain reverses it via [`revert_approval_within`]).
pub fn apply_approval_within(
    tx: &Transaction<'_>,
    app_id: &AppId,
    approval: Approval,
    origin: &str,
) -> Result<EventId> {
    let previous = current_approval_within(tx, app_id)?;
    set_approval_within(tx, app_id, approval)?;

    let old_json = previous
        .map(|value| serde_json::to_string(&value))
        .transpose()?;
    let new_json = serde_json::to_string(&approval)?;
    let event_id = EventId::new();
    let change_id = change_log::record_within(
        tx,
        "background.approval",
        app_id.as_str(),
        old_json.as_deref(),
        Some(&new_json),
        event_id,
    )?;
    let message = match approval {
        Approval::Approved => format!("{app_id} was approved to run in the background."),
        Approval::Denied => format!("{app_id} was denied permission to run in the background."),
        Approval::Undecided => format!("{app_id}'s background permission was reset to undecided."),
    };
    append_within(
        tx,
        &Event {
            id: event_id,
            at_ms: templar_state::now_ms(),
            kind: EventKind::AutostartDecision,
            app_id: Some(app_id.clone()),
            instance_id: None,
            message,
            cause: Some(origin.to_owned()),
            actions: vec![EventAction {
                label: "Undo".into(),
                command: change_log::undo_command(change_id),
            }],
            reversible: true,
        },
    )?;
    Ok(event_id)
}

/// Reverse a `background.approval` change recorded in the shared `change_log`
/// (`History1.Undo`). `key` is the app ID `apply_approval_within` used; `old_value_json` is
/// `None` when the app had no stored approval (undecided) before the change being undone.
pub fn revert_approval_within(
    tx: &Transaction<'_>,
    key: &str,
    old_value_json: Option<&str>,
) -> Result<()> {
    let app_id: AppId = key
        .parse()
        .with_context(|| format!("invalid app id in change_log key: {key:?}"))?;
    let approval = match old_value_json {
        Some(json) => serde_json::from_str(json).context("parsing a stored background approval")?,
        None => Approval::Undecided,
    };
    set_approval_within(tx, &app_id, approval)
}

/// Load every persisted declaration (what the last autostart scan found this application
/// requesting). An app with no row here has never been seen declaring background capability.
pub fn load_declarations(store: &Store) -> Result<Vec<AutostartEntry>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT app_id, role, requested FROM background_declarations")
        .context("preparing background_declarations query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("reading background_declarations")?;
    let mut entries = Vec::new();
    for row in rows {
        let (app_id, role, requested_json) = row?;
        let app_id: AppId = app_id
            .parse()
            .with_context(|| format!("invalid app id in background_declarations: {app_id:?}"))?;
        let role = role
            .map(|value| {
                ProviderRole::ALL
                    .iter()
                    .copied()
                    .find(|role| role.as_str() == value)
                    .with_context(|| {
                        format!("invalid provider role in background_declarations: {value:?}")
                    })
            })
            .transpose()?;
        let requested: Vec<BackgroundPermission> = serde_json::from_str(&requested_json)
            .context("parsing declared background permissions")?;
        entries.push(AutostartEntry {
            app_id,
            role,
            requested,
        });
    }
    Ok(entries)
}

/// Persist (or refresh) one declaration; called once per discovered entry each autostart scan.
/// Not itself a user decision, so it carries no history/undo — only `apply_approval_within` does.
pub fn save_declaration_within(tx: &Transaction<'_>, entry: &AutostartEntry) -> Result<()> {
    let requested_json =
        serde_json::to_string(&entry.requested).context("encoding declared permissions")?;
    let role = entry.role.map(ProviderRole::as_str);
    tx.execute(
        "INSERT INTO background_declarations (app_id, role, requested) VALUES (?1, ?2, ?3)
         ON CONFLICT (app_id) DO UPDATE SET role = excluded.role, requested = excluded.requested",
        params![entry.app_id.as_str(), role, requested_json],
    )
    .context("persisting background declaration")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::ProviderCapability;

    #[test]
    fn duplicate_role_providers_do_not_both_start() {
        let mut providers = ProviderRegistry::default();
        let a: AppId = "org.example.A".parse().unwrap();
        let b: AppId = "org.example.B".parse().unwrap();
        let c: AppId = "org.example.Sync".parse().unwrap();
        providers.declare(
            a.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );
        providers.declare(
            b.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 50,
            },
        );

        let entries = vec![
            (
                AutostartEntry {
                    app_id: a.clone(),
                    role: Some(ProviderRole::NetworkStatusUi),
                    requested: vec![],
                },
                Approval::Undecided,
            ),
            (
                AutostartEntry {
                    app_id: b.clone(),
                    role: Some(ProviderRole::NetworkStatusUi),
                    requested: vec![],
                },
                Approval::Approved,
            ),
            (
                AutostartEntry {
                    app_id: c.clone(),
                    role: None,
                    requested: vec![BackgroundPermission::ContinuousBackground],
                },
                Approval::Undecided,
            ),
        ];
        let plan = plan(&entries, &providers);
        assert_eq!(plan[0], (a, Verdict::Launch));
        assert_eq!(
            plan[1],
            (
                b,
                Verdict::SkippedNotSelectedProvider {
                    role: ProviderRole::NetworkStatusUi
                }
            )
        );
        assert_eq!(plan[2], (c, Verdict::SkippedUndecided));
    }

    #[test]
    fn approvals_persist_and_undecided_is_absence_of_a_row() {
        let store = Store::in_memory().unwrap();
        let app: AppId = "org.example.Sync".parse().unwrap();

        assert_eq!(
            approval_for(&load_approvals(&store).unwrap(), &app),
            Approval::Undecided
        );

        store
            .transaction(|tx| set_approval_within(tx, &app, Approval::Denied))
            .unwrap();
        assert_eq!(
            approval_for(&load_approvals(&store).unwrap(), &app),
            Approval::Denied
        );

        // Setting Undecided again removes the row rather than storing a no-op value.
        store
            .transaction(|tx| set_approval_within(tx, &app, Approval::Undecided))
            .unwrap();
        assert!(load_approvals(&store).unwrap().is_empty());
    }

    #[test]
    fn background_permission_round_trips_through_its_kebab_name() {
        for permission in [
            BackgroundPermission::LoginAutostart,
            BackgroundPermission::ContinuousBackground,
            BackgroundPermission::NetworkWhileHidden,
            BackgroundPermission::GlobalShortcuts,
            BackgroundPermission::ScreenCapture,
            BackgroundPermission::InputMonitoring,
            BackgroundPermission::Notifications,
            BackgroundPermission::RemovableMedia,
        ] {
            assert_eq!(
                BackgroundPermission::parse(permission.as_str()),
                Some(permission)
            );
        }
        assert_eq!(BackgroundPermission::parse("not-a-real-permission"), None);
    }

    #[test]
    fn declarations_persist_and_reload_including_the_role() {
        let store = Store::in_memory().unwrap();
        let app: AppId = "org.example.Sync".parse().unwrap();
        let entry = AutostartEntry {
            app_id: app.clone(),
            role: Some(ProviderRole::NotificationServer),
            requested: vec![
                BackgroundPermission::ContinuousBackground,
                BackgroundPermission::Notifications,
            ],
        };
        store
            .transaction(|tx| save_declaration_within(tx, &entry))
            .unwrap();

        let loaded = load_declarations(&store).unwrap();
        assert_eq!(loaded, vec![entry.clone()]);

        // Re-saving (a later scan) overwrites rather than duplicating.
        let updated = AutostartEntry {
            requested: vec![BackgroundPermission::ScreenCapture],
            ..entry
        };
        store
            .transaction(|tx| save_declaration_within(tx, &updated))
            .unwrap();
        let loaded = load_declarations(&store).unwrap();
        assert_eq!(loaded, vec![updated]);
    }

    #[test]
    fn apply_approval_persists_records_history_and_undo_reverses_it() {
        let store = Store::in_memory().unwrap();
        let app: AppId = "org.example.Sync".parse().unwrap();

        let event_id = store
            .transaction(|tx| apply_approval_within(tx, &app, Approval::Approved, "user"))
            .unwrap();
        assert_eq!(
            approval_for(&load_approvals(&store).unwrap(), &app),
            Approval::Approved
        );

        let connection = store.lock();
        let message: String = connection
            .query_row(
                "SELECT message FROM history WHERE event_id = ?1",
                params![event_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(message.contains("approved"), "{message}");
        let (domain, key, old_value): (String, String, Option<String>) = connection
            .query_row(
                "SELECT domain, key, old_value FROM change_log WHERE event_id = ?1",
                params![event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        drop(connection);
        assert_eq!(domain, "background.approval");
        assert_eq!(key, app.as_str());
        assert_eq!(old_value, None, "no approval was stored before this change");

        store
            .transaction(|tx| revert_approval_within(tx, &key, old_value.as_deref()))
            .unwrap();
        assert_eq!(
            approval_for(&load_approvals(&store).unwrap(), &app),
            Approval::Undecided,
            "reverting a change with no prior approval returns to undecided"
        );
    }

    #[test]
    fn undo_restores_a_previous_approval_not_just_undecided() {
        let store = Store::in_memory().unwrap();
        let app: AppId = "org.example.Sync".parse().unwrap();

        store
            .transaction(|tx| apply_approval_within(tx, &app, Approval::Approved, "user"))
            .unwrap();
        let second_event = store
            .transaction(|tx| apply_approval_within(tx, &app, Approval::Denied, "user"))
            .unwrap();

        let old_value: Option<String> = store
            .lock()
            .query_row(
                "SELECT old_value FROM change_log WHERE event_id = ?1",
                params![second_event.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_value.as_deref(), Some("\"approved\""));

        store
            .transaction(|tx| revert_approval_within(tx, app.as_str(), old_value.as_deref()))
            .unwrap();
        assert_eq!(
            approval_for(&load_approvals(&store).unwrap(), &app),
            Approval::Approved
        );
    }
}
