//! Declarative system-provider selection: one selected provider per exclusive role.

use crate::{
    change_log,
    history::{Event, EventAction, EventId, EventKind, append_within},
};
use anyhow::{Context, Result};
use lege_state::Store;
use lege_types::{AppId, ProviderCapability, ProviderRole};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A capability declared by an installed application (from its manifest or autostart entry).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub app_id: AppId,
    pub capability: ProviderCapability,
}

/// Registry of declarations plus user/distribution selections.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistry {
    pub declarations: Vec<Declaration>,
    /// Explicit selection per role (user choice or distribution policy).
    pub selections: BTreeMap<ProviderRole, AppId>,
}

/// Why a provider is (not) selected — reportable by `lege doctor` and the settings UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Resolution {
    /// Explicitly selected and installed.
    Selected { app_id: AppId },
    /// No explicit selection: highest-priority declaration wins.
    DefaultedByPriority {
        app_id: AppId,
        competitors: Vec<AppId>,
    },
    /// Selected app is not installed / no longer declares the role.
    SelectionStale { app_id: AppId },
    /// Nothing declares this role.
    NoProvider,
}

impl ProviderRegistry {
    pub fn declare(&mut self, app_id: AppId, capability: ProviderCapability) {
        self.declarations
            .retain(|d| !(d.app_id == app_id && d.capability.role == capability.role));
        self.declarations.push(Declaration { app_id, capability });
    }

    pub fn undeclare_app(&mut self, app_id: &AppId) {
        self.declarations.retain(|d| &d.app_id != app_id);
    }

    pub fn select(&mut self, role: ProviderRole, app_id: AppId) {
        self.selections.insert(role, app_id);
    }

    /// Drop an explicit selection, returning the role to whatever priority decides.
    ///
    /// Not the same as selecting the application that priority would have picked: that would
    /// pin today's winner, so a later-installed higher-priority provider would not take over.
    pub fn deselect(&mut self, role: ProviderRole) -> Option<AppId> {
        self.selections.remove(&role)
    }

    /// Every application that declares `role`, best first — the same order [`Self::resolve`]
    /// picks a default from, so a chooser built on this shows the default at the top.
    ///
    /// Priority is carried alongside because it is the reason for the order; a chooser that
    /// showed the order without it would be asking a person to trust an arrangement it will not
    /// explain.
    pub fn candidates(&self, role: ProviderRole) -> Vec<(AppId, u32)> {
        self.declaring(role)
            .into_iter()
            .map(|declaration| (declaration.app_id.clone(), declaration.capability.priority))
            .collect()
    }

    fn declaring(&self, role: ProviderRole) -> Vec<&Declaration> {
        let mut list: Vec<&Declaration> = self
            .declarations
            .iter()
            .filter(|d| d.capability.role == role)
            .collect();
        list.sort_by(|a, b| {
            b.capability
                .priority
                .cmp(&a.capability.priority)
                .then_with(|| a.app_id.cmp(&b.app_id))
        });
        list
    }

    /// Resolve the single provider for `role`.
    pub fn resolve(&self, role: ProviderRole) -> Resolution {
        let declaring = self.declaring(role);
        if let Some(selected) = self.selections.get(&role) {
            return if declaring.iter().any(|d| &d.app_id == selected) {
                Resolution::Selected {
                    app_id: selected.clone(),
                }
            } else {
                Resolution::SelectionStale {
                    app_id: selected.clone(),
                }
            };
        }
        match declaring.split_first() {
            None => Resolution::NoProvider,
            Some((winner, rest)) => Resolution::DefaultedByPriority {
                app_id: winner.app_id.clone(),
                competitors: rest.iter().map(|d| d.app_id.clone()).collect(),
            },
        }
    }

    /// The application allowed to fill `role`, if any.
    pub fn provider(&self, role: ProviderRole) -> Option<AppId> {
        match self.resolve(role) {
            Resolution::Selected { app_id } | Resolution::DefaultedByPriority { app_id, .. } => {
                Some(app_id)
            }
            Resolution::SelectionStale { .. } | Resolution::NoProvider => None,
        }
    }

    /// Whether `app_id` may autostart for `role` (exactly one may).
    pub fn may_autostart(&self, app_id: &AppId, role: ProviderRole) -> bool {
        self.provider(role).as_ref() == Some(app_id)
    }

    /// Full report for every role.
    pub fn report(&self) -> Vec<(ProviderRole, Resolution)> {
        ProviderRole::ALL
            .iter()
            .map(|role| (*role, self.resolve(*role)))
            .collect()
    }
}

fn role_by_name(name: &str) -> Result<ProviderRole> {
    ProviderRole::ALL
        .iter()
        .copied()
        .find(|role| role.as_str() == name)
        .with_context(|| format!("unknown provider role {name:?}"))
}

/// Load every declaration and selection persisted so far. Missing/empty tables yield the
/// default (empty) registry, which is exactly first-run behaviour.
pub fn load(store: &Store) -> Result<ProviderRegistry> {
    let connection = store.lock();
    let mut registry = ProviderRegistry::default();

    let mut declarations = connection
        .prepare("SELECT app_id, role, priority FROM provider_declarations")
        .context("preparing provider_declarations query")?;
    let rows = declarations
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })
        .context("reading provider_declarations")?;
    for row in rows {
        let (app_id, role, priority) = row?;
        let app_id: AppId = app_id
            .parse()
            .with_context(|| format!("invalid app id in provider_declarations: {app_id:?}"))?;
        let role = role_by_name(&role)?;
        registry.declare(app_id, ProviderCapability { role, priority });
    }
    drop(declarations);

    let mut selections = connection
        .prepare("SELECT role, app_id FROM provider_selections")
        .context("preparing provider_selections query")?;
    let rows = selections
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("reading provider_selections")?;
    for row in rows {
        let (role, app_id) = row?;
        let role = role_by_name(&role)?;
        let app_id: AppId = app_id
            .parse()
            .with_context(|| format!("invalid app id in provider_selections: {app_id:?}"))?;
        registry.select(role, app_id);
    }
    Ok(registry)
}

/// Persist one declaration (upsert on `(app_id, role)`).
pub fn declare_within(
    tx: &Transaction<'_>,
    app_id: &AppId,
    capability: &ProviderCapability,
) -> Result<()> {
    tx.execute(
        "INSERT INTO provider_declarations (app_id, role, priority) VALUES (?1, ?2, ?3)
         ON CONFLICT (app_id, role) DO UPDATE SET priority = excluded.priority",
        params![
            app_id.as_str(),
            capability.role.as_str(),
            capability.priority
        ],
    )
    .context("persisting provider declaration")?;
    Ok(())
}

/// Remove every declaration `app_id` made (the application was uninstalled or is exiting a role).
pub fn undeclare_within(tx: &Transaction<'_>, app_id: &AppId) -> Result<()> {
    tx.execute(
        "DELETE FROM provider_declarations WHERE app_id = ?1",
        params![app_id.as_str()],
    )
    .context("removing provider declarations")?;
    Ok(())
}

/// Persist the selection for one role; returns the previous selection, if any (for the undo log).
pub fn select_within(
    tx: &Transaction<'_>,
    role: ProviderRole,
    app_id: &AppId,
) -> Result<Option<AppId>> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT app_id FROM provider_selections WHERE role = ?1",
            params![role.as_str()],
            |row| row.get(0),
        )
        .optional()
        .context("reading previous provider selection")?;
    tx.execute(
        "INSERT INTO provider_selections (role, app_id) VALUES (?1, ?2)
         ON CONFLICT (role) DO UPDATE SET app_id = excluded.app_id",
        params![role.as_str(), app_id.as_str()],
    )
    .context("persisting provider selection")?;
    previous
        .map(|value| value.parse::<AppId>())
        .transpose()
        .context("invalid app id previously stored as a provider selection")
}

/// Change who fills `role` — to `choice`, or back to automatic when it is `None` — and journal
/// it, so `History1` shows the change and `History1.Undo` can put it back.
///
/// The `providers.selection` undo applier has existed in the daemon's dispatcher since the
/// change_log was introduced; nothing wrote an entry for it to reverse. This is that writer, and
/// it is the only path a caller should take to change a selection: `select_within` and
/// `deselect_within` persist the row but leave no record, so a selection made through them is
/// one a person cannot see happen and cannot take back.
///
/// `None` is a real old value in the journal — it means the role was on automatic — which is
/// exactly what [`revert_selection_within`] reads to decide between restoring a pin and removing
/// one.
pub fn apply_selection_within(
    tx: &Transaction<'_>,
    role: ProviderRole,
    choice: Option<&AppId>,
    origin: &str,
) -> Result<EventId> {
    let previous = match choice {
        Some(app_id) => select_within(tx, role, app_id)?,
        None => deselect_within(tx, role)?,
    };

    let old_json = previous
        .as_ref()
        .map(|value| serde_json::to_string(value.as_str()))
        .transpose()
        .context("encoding the previous provider selection")?;
    let new_json = choice
        .map(|app_id| serde_json::to_string(app_id.as_str()))
        .transpose()
        .context("encoding the new provider selection")?;
    let event_id = EventId::new();
    let change_id = change_log::record_within(
        tx,
        "providers.selection",
        role.as_str(),
        old_json.as_deref(),
        new_json.as_deref(),
        event_id,
    )?;
    let message = match (choice, previous.as_ref()) {
        (Some(app_id), _) => format!("{app_id} was selected to fill the {role} role."),
        (None, Some(previous)) => format!(
            "{role} was returned to choosing automatically; {previous} is no longer pinned."
        ),
        (None, None) => format!("{role} was already choosing automatically."),
    };
    append_within(
        tx,
        &Event {
            id: event_id,
            at_ms: lege_state::now_ms(),
            kind: EventKind::ProviderChange,
            app_id: choice.cloned().or(previous),
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

/// Remove the explicit selection for `role`, returning the one that was there.
///
/// Returning the previous value is what lets the caller journal the change: `None` is a
/// meaningful old value (the role was already on automatic) and has to survive into `change_log`
/// as such, because that is what [`revert_selection_within`] reads to undo this.
pub fn deselect_within(tx: &Transaction<'_>, role: ProviderRole) -> Result<Option<AppId>> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT app_id FROM provider_selections WHERE role = ?1",
            params![role.as_str()],
            |row| row.get(0),
        )
        .optional()
        .context("reading previous provider selection")?;
    tx.execute(
        "DELETE FROM provider_selections WHERE role = ?1",
        params![role.as_str()],
    )
    .context("removing a provider selection")?;
    previous
        .map(|value| value.parse::<AppId>())
        .transpose()
        .context("invalid app id previously stored as a provider selection")
}

/// Reverse a selection change recorded in the shared `change_log` (`History1.Undo`).
/// `old_value_json` is a JSON string (the previous `AppId`) or `None` if the role had no
/// selection before the change being undone.
pub fn revert_selection_within(
    tx: &Transaction<'_>,
    registry: &mut ProviderRegistry,
    role: ProviderRole,
    old_value_json: Option<&str>,
) -> Result<()> {
    if let Some(json) = old_value_json {
        let raw: String =
            serde_json::from_str(json).context("parsing a stored provider selection")?;
        let app_id: AppId = raw
            .parse()
            .context("stored provider selection is not a valid AppId")?;
        tx.execute(
            "INSERT INTO provider_selections (role, app_id) VALUES (?1, ?2)
             ON CONFLICT (role) DO UPDATE SET app_id = excluded.app_id",
            params![role.as_str(), app_id.as_str()],
        )
        .context("restoring a provider_selections row")?;
        registry.select(role, app_id);
    } else {
        tx.execute(
            "DELETE FROM provider_selections WHERE role = ?1",
            params![role.as_str()],
        )
        .context("removing a provider_selections row")?;
        registry.selections.remove(&role);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(role: ProviderRole, priority: u32) -> ProviderCapability {
        ProviderCapability { role, priority }
    }

    #[test]
    fn candidates_are_listed_best_first_with_the_priority_that_ordered_them() {
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));

        assert_eq!(
            registry.candidates(ProviderRole::NetworkStatusUi),
            vec![(tray, 100), (applet, 50)]
        );
    }

    #[test]
    fn a_role_nothing_declares_has_no_candidates() {
        let registry = ProviderRegistry::default();
        assert!(
            registry
                .candidates(ProviderRole::NetworkStatusUi)
                .is_empty()
        );
    }

    #[test]
    fn candidates_are_the_declarations_not_the_selection() {
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));
        registry.select(ProviderRole::NetworkStatusUi, applet.clone());

        // Selecting the lower-priority one must not reorder or shorten the choice a person is
        // offered — that would hide the alternative they just moved away from.
        assert_eq!(
            registry.candidates(ProviderRole::NetworkStatusUi),
            vec![(tray, 100), (applet, 50)]
        );
    }

    #[test]
    fn deselecting_returns_the_role_to_priority_rather_than_pinning_the_winner() {
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));
        registry.select(ProviderRole::NetworkStatusUi, applet.clone());
        assert_eq!(
            registry.deselect(ProviderRole::NetworkStatusUi),
            Some(applet)
        );

        // A higher-priority provider installed afterwards now wins, which is the whole point of
        // going back to automatic.
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));
        assert_eq!(registry.provider(ProviderRole::NetworkStatusUi), Some(tray));
    }

    #[test]
    fn deselecting_a_role_that_was_already_automatic_reports_no_previous_selection() {
        let mut registry = ProviderRegistry::default();
        assert_eq!(registry.deselect(ProviderRole::NetworkStatusUi), None);
    }

    #[test]
    fn exactly_one_network_indicator_may_autostart() {
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));

        assert!(registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));
        assert!(!registry.may_autostart(&applet, ProviderRole::NetworkStatusUi));
        assert!(
            matches!(registry.resolve(ProviderRole::NetworkStatusUi), Resolution::DefaultedByPriority { competitors, .. } if competitors == vec![applet.clone()])
        );

        registry.select(ProviderRole::NetworkStatusUi, applet.clone());
        assert!(registry.may_autostart(&applet, ProviderRole::NetworkStatusUi));
        assert!(!registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));

        registry.undeclare_app(&applet); // uninstalled
        assert_eq!(
            registry.resolve(ProviderRole::NetworkStatusUi),
            Resolution::SelectionStale { app_id: applet }
        );
        assert_eq!(registry.provider(ProviderRole::NetworkStatusUi), None);
        assert_eq!(
            registry.resolve(ProviderRole::Greeter),
            Resolution::NoProvider
        );
    }

    #[test]
    fn selecting_through_the_journal_leaves_a_reversible_event() {
        let store = Store::in_memory().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();

        store
            .transaction(|tx| {
                declare_within(tx, &tray, &cap(ProviderRole::NetworkStatusUi, 100))?;
                declare_within(tx, &applet, &cap(ProviderRole::NetworkStatusUi, 50))?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, Some(&applet), "test")?;
                Ok(())
            })
            .unwrap();

        let registry = load(&store).unwrap();
        assert_eq!(
            registry.provider(ProviderRole::NetworkStatusUi),
            Some(applet.clone())
        );

        let events = {
            use crate::history::HistoryStore;
            crate::history::SqliteHistory::new(store.clone())
                .recent(10)
                .unwrap()
        };
        let event = events
            .iter()
            .find(|event| event.kind == crate::history::EventKind::ProviderChange)
            .expect("the selection was recorded");
        assert!(
            event.reversible,
            "a selection a person made must be undoable"
        );
        assert!(event.message.contains("org.gnome.NmApplet"));
        assert_eq!(event.actions.len(), 1, "one Undo action");
    }

    #[test]
    fn undoing_a_selection_restores_the_one_that_was_there_before_it() {
        let store = Store::in_memory().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let mut registry = ProviderRegistry::default();
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));

        store
            .transaction(|tx| {
                declare_within(tx, &tray, &cap(ProviderRole::NetworkStatusUi, 100))?;
                declare_within(tx, &applet, &cap(ProviderRole::NetworkStatusUi, 50))?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, Some(&tray), "test")?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, Some(&applet), "test")?;
                Ok(())
            })
            .unwrap();

        // What the dispatcher does for a `providers.selection` entry, with the old value the
        // second change journalled.
        let old_value = serde_json::to_string(tray.as_str()).unwrap();
        store
            .transaction(|tx| {
                revert_selection_within(
                    tx,
                    &mut registry,
                    ProviderRole::NetworkStatusUi,
                    Some(&old_value),
                )
            })
            .unwrap();

        assert_eq!(
            load(&store).unwrap().selections[&ProviderRole::NetworkStatusUi],
            tray
        );
    }

    #[test]
    fn undoing_the_first_selection_a_role_ever_had_returns_it_to_automatic() {
        let store = Store::in_memory().unwrap();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let mut registry = ProviderRegistry::default();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));

        store
            .transaction(|tx| {
                declare_within(tx, &applet, &cap(ProviderRole::NetworkStatusUi, 50))?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, Some(&applet), "test")?;
                // No old value: the role had never been pinned before this.
                revert_selection_within(tx, &mut registry, ProviderRole::NetworkStatusUi, None)
            })
            .unwrap();

        assert!(load(&store).unwrap().selections.is_empty());
    }

    #[test]
    fn returning_a_role_to_automatic_is_itself_recorded_and_undoable() {
        let store = Store::in_memory().unwrap();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();

        store
            .transaction(|tx| {
                declare_within(tx, &applet, &cap(ProviderRole::NetworkStatusUi, 50))?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, Some(&applet), "test")?;
                apply_selection_within(tx, ProviderRole::NetworkStatusUi, None, "test")?;
                Ok(())
            })
            .unwrap();

        assert!(load(&store).unwrap().selections.is_empty());
        let events = {
            use crate::history::HistoryStore;
            crate::history::SqliteHistory::new(store.clone())
                .recent(10)
                .unwrap()
        };
        let latest = events
            .iter()
            .find(|event| event.kind == crate::history::EventKind::ProviderChange)
            .expect("the deselection was recorded");
        assert!(latest.reversible);
        assert!(latest.message.contains("choosing automatically"));
    }

    #[test]
    fn declarations_and_selections_persist_and_reload() {
        let store = Store::in_memory().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();

        store
            .transaction(|tx| {
                declare_within(tx, &tray, &cap(ProviderRole::NetworkStatusUi, 100))?;
                declare_within(tx, &applet, &cap(ProviderRole::NetworkStatusUi, 50))?;
                let previous = select_within(tx, ProviderRole::NetworkStatusUi, &applet)?;
                assert_eq!(previous, None);
                Ok(())
            })
            .unwrap();

        let registry = load(&store).unwrap();
        assert!(registry.may_autostart(&applet, ProviderRole::NetworkStatusUi));
        assert!(!registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));

        store
            .transaction(|tx| {
                let previous = select_within(tx, ProviderRole::NetworkStatusUi, &tray)?;
                assert_eq!(previous, Some(applet.clone()));
                undeclare_within(tx, &applet)
            })
            .unwrap();

        let registry = load(&store).unwrap();
        assert!(registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));
        assert_eq!(
            registry.resolve(ProviderRole::NetworkStatusUi),
            Resolution::Selected { app_id: tray }
        );
    }

    #[test]
    fn revert_selection_restores_the_previous_app_or_clears_the_row() {
        let store = Store::in_memory().unwrap();
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(
            applet.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 50,
            },
        );
        registry.declare(
            tray.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );

        let previous_json = store
            .transaction(|tx| {
                let previous = select_within(tx, ProviderRole::NetworkStatusUi, &applet)?;
                registry.select(ProviderRole::NetworkStatusUi, applet.clone());
                Ok(previous.map(|id| serde_json::to_string(id.as_str()).unwrap()))
            })
            .unwrap();
        assert_eq!(previous_json, None);

        store
            .transaction(|tx| {
                let previous = select_within(tx, ProviderRole::NetworkStatusUi, &tray)?;
                registry.select(ProviderRole::NetworkStatusUi, tray.clone());
                assert_eq!(previous, Some(applet.clone()));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            registry.provider(ProviderRole::NetworkStatusUi),
            Some(tray.clone())
        );

        // Undo the second selection: back to the applet.
        store
            .transaction(|tx| {
                revert_selection_within(
                    tx,
                    &mut registry,
                    ProviderRole::NetworkStatusUi,
                    Some(&serde_json::to_string(applet.as_str()).unwrap()),
                )
            })
            .unwrap();
        assert_eq!(
            registry.provider(ProviderRole::NetworkStatusUi),
            Some(applet)
        );

        // Undo the first selection too: no selection at all (defaults by priority again).
        store
            .transaction(|tx| {
                revert_selection_within(tx, &mut registry, ProviderRole::NetworkStatusUi, None)
            })
            .unwrap();
        assert!(
            !registry
                .selections
                .contains_key(&ProviderRole::NetworkStatusUi)
        );
        assert_eq!(
            load(&store)
                .unwrap()
                .selections
                .get(&ProviderRole::NetworkStatusUi),
            None
        );
    }
}
