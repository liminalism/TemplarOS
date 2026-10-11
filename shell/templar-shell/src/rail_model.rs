//! Saved navigation targets reconciled with the authoritative running-task projection.
use crate::{
    app_search::AppEntry,
    taskbar::{Entry, EntryWindow, Lane, LaneMode, Rows},
};
use std::collections::{HashMap, HashSet};
use templar_types::launcher::{FavoriteId, FavoriteTarget, LauncherPreferences};
use templar_window::{WindowId, task::TaskKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailSlot {
    Running(TaskKey),
    Favorite(FavoriteId),
    Launcher,
}

/// Resolve a selected installed copy and a logical id through the same catalogue projection.
pub fn application_for_id<'a>(
    id: &str,
    apps: &'a [AppEntry],
    variants: &HashMap<String, Vec<(String, String, String)>>,
) -> Option<&'a AppEntry> {
    apps.iter().find(|app| {
        app.id.as_str() == id
            || !app.logical.is_empty()
                && (app.logical == id
                    || variants
                        .get(&app.logical)
                        .is_some_and(|copies| copies.iter().any(|copy| copy.0 == id)))
    })
}
pub fn matches_application(
    id: &str,
    candidate: &str,
    apps: &[AppEntry],
    variants: &HashMap<String, Vec<(String, String, String)>>,
) -> bool {
    candidate == id
        || application_for_id(id, apps, variants).is_some_and(|app| {
            candidate == app.id.as_str()
                || !app.logical.is_empty()
                    && (candidate == app.logical
                        || variants
                            .get(&app.logical)
                            .is_some_and(|copies| copies.iter().any(|copy| copy.0 == candidate)))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RailEntry {
    pub slot: RailSlot,
    pub task: Option<Entry>,
    pub icon_id: Option<String>,
    pub label: String,
    pub lane: Lane,
    pub profile_window: Option<WindowId>,
}
impl From<Entry> for RailEntry {
    fn from(task: Entry) -> Self {
        Self {
            slot: RailSlot::Running(task.unit.key.clone()),
            icon_id: task.app_id.clone(),
            label: task.label.clone(),
            lane: task.lane,
            profile_window: None,
            task: Some(task),
        }
    }
}
impl RailEntry {
    pub fn key(&self) -> String {
        match &self.slot {
            RailSlot::Running(key) => key.wire(),
            RailSlot::Favorite(id) => format!("favorite:{id}"),
            RailSlot::Launcher => "launcher".into(),
        }
    }
    pub fn windows(&self) -> &[EntryWindow] {
        self.task.as_ref().map_or(&[], |task| &task.windows)
    }
    pub fn active(&self) -> bool {
        self.task.as_ref().is_some_and(|task| task.active)
    }
    pub fn accent(&self) -> Option<u32> {
        self.task.as_ref().and_then(Entry::accent)
    }
    pub fn visible(&self) -> usize {
        self.task.as_ref().map_or(0, Entry::visible)
    }
    pub fn most_recent(&self) -> Option<WindowId> {
        self.profile_window
            .or_else(|| self.task.as_ref().and_then(Entry::most_recent))
    }
    pub fn all_parked(&self) -> bool {
        self.task.as_ref().is_some_and(Entry::all_parked)
    }
    pub fn is_parked(&self, window: WindowId) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| task.is_parked(window))
    }
    pub fn is_group(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| matches!(task.unit.key, TaskKey::Group(_)))
    }
    pub fn chip_action(&self) -> Option<templar_window::task::ChipAction> {
        self.task
            .as_ref()
            .map(|task| templar_window::task::chip_action(&task.unit))
    }
}

/// Explicit groups first, ordered saved targets next, then other tasks in first-seen order.
/// A favorite that starts/stops keeps its own slot. Groups and unknown claims remain untouched.
pub fn reconcile(
    tasks: Vec<Entry>,
    preferences: &LauncherPreferences,
    apps: &[AppEntry],
    variants: &HashMap<String, Vec<(String, String, String)>>,
    profile_windows: &HashMap<FavoriteId, WindowId>,
) -> Vec<RailEntry> {
    let mut consumed = HashSet::new();
    let mut pinned_apps = HashSet::new();
    let mut result: Vec<RailEntry> = tasks
        .iter()
        .filter(|task| matches!(task.unit.key, TaskKey::Group(_)))
        .cloned()
        .map(Into::into)
        .collect();
    for (index, task) in tasks.iter().enumerate() {
        if matches!(task.unit.key, TaskKey::Group(_)) {
            consumed.insert(index);
        }
    }
    for favorite in &preferences.favorites {
        let id = favorite.target.app_id();
        let app = application_for_id(id, apps, variants);
        if matches!(favorite.target, FavoriteTarget::App { .. })
            && !pinned_apps.insert(
                app.map_or(id, |app| {
                    if app.logical.is_empty() {
                        app.id.as_str()
                    } else {
                        &app.logical
                    }
                })
                .to_owned(),
            )
        {
            continue;
        }
        let matches = |candidate: &str| matches_application(id, candidate, apps, variants);
        let task = match &favorite.target {
            FavoriteTarget::App { .. } => tasks.iter().enumerate().find(|(index, task)| !consumed.contains(index) && matches!(&task.unit.key, TaskKey::App(candidate) if matches(candidate.as_str()))).map(|(index, task)| { consumed.insert(index); task.clone() }),
            FavoriteTarget::Terminal { .. } => None,
        };
        result.push(RailEntry {
            slot: RailSlot::Favorite(favorite.id.clone()),
            label: if favorite.label.is_empty() {
                app.map_or(id, AppEntry::label).to_owned()
            } else {
                favorite.label.clone()
            },
            icon_id: Some(app.map_or(id, |app| app.id.as_str()).to_owned()),
            lane: if matches!(favorite.target, FavoriteTarget::Terminal { .. })
                || app.is_some_and(|app| {
                    app.categories
                        .iter()
                        .any(|category| category == crate::taskbar::ACTIVE_LANE_CATEGORY)
                }) {
                Lane::Active
            } else {
                task.as_ref().map_or(Lane::Latent, |task| task.lane)
            },
            profile_window: profile_windows.get(&favorite.id).copied(),
            task,
        });
    }
    result.extend(
        tasks
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !consumed.contains(index))
            .map(|(_, task)| RailEntry::from(task)),
    );
    result.push(RailEntry {
        slot: RailSlot::Launcher,
        task: None,
        icon_id: None,
        label: "Search applications and files".into(),
        lane: Lane::Latent,
        profile_window: None,
    });
    result
}

pub fn rows_of(entries: &[RailEntry], mode: LaneMode, capacity: usize) -> Option<Rows> {
    if mode != LaneMode::Rows {
        return None;
    }
    let active = entries
        .iter()
        .filter(|entry| entry.lane == Lane::Active)
        .count();
    let latent = entries.len() - active;
    (active > 0 && latent > 0 && active <= capacity && latent <= capacity)
        .then_some(Rows { active, latent })
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::launcher::Favorite;
    use templar_window::{Layer, Rect, ShellToplevel};
    fn top(id: u64, app: &str, verified: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: verified.then(|| app.parse().unwrap()),
            claimed_app_id: Some(app.into()),
            instance_id: None,
            title: app.into(),
            verified_identity: verified,
            activated: false,
            mru_rank: 0,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 100, 100),
            layer: Layer::Normal,
            group: None,
            tab_count: None,
        }
    }
    #[test]
    fn favorite_occupies_one_stable_slot_when_running_and_unknown_claim_stays_separate() {
        let prefs = LauncherPreferences {
            favorites: vec![Favorite {
                id: "browser".into(),
                label: "Browser".into(),
                target: FavoriteTarget::App {
                    app_id: "org.example.Browser".into(),
                },
            }],
            ..LauncherPreferences::default()
        };
        let idle = reconcile(vec![], &prefs, &[], &HashMap::new(), &HashMap::new());
        let live = reconcile(
            crate::taskbar::entries(&[
                top(1, "org.example.Browser", true),
                top(2, "org.example.Browser", false),
            ]),
            &prefs,
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(live[0].slot, idle[0].slot);
        assert_eq!(live[0].windows().len(), 1);
        assert!(matches!(
            live[1].slot,
            RailSlot::Running(TaskKey::Unknown(_))
        ));
        assert_eq!(live.len(), 3);
    }
    #[test]
    fn profiles_stay_distinct_and_groups_keep_their_actual_windows() {
        let mut grouped = top(1, "org.example.Terminal", true);
        grouped.group = Some(templar_window::GroupInfo {
            group_id: 7,
            name: "Work".into(),
            accent: 1,
        });
        let profiles = LauncherPreferences {
            favorites: [("one", "L3dvcmsvb25l"), ("two", "L3dvcmsvdHdv")]
                .into_iter()
                .map(|(id, cwd)| Favorite {
                    id: id.into(),
                    label: id.into(),
                    target: FavoriteTarget::Terminal {
                        app_id: "org.example.Terminal".into(),
                        cwd_b64: cwd.into(),
                        arguments: vec![],
                    },
                })
                .collect(),
            ..LauncherPreferences::default()
        };
        let entries = reconcile(
            crate::taskbar::entries(&[grouped]),
            &profiles,
            &[],
            &HashMap::new(),
            &HashMap::from([("one".into(), WindowId(1))]),
        );
        assert!(entries[0].is_group());
        assert_eq!(entries[0].windows()[0].id, WindowId(1));
        assert_eq!(entries[1].most_recent(), Some(WindowId(1)));
        assert_eq!(entries[2].most_recent(), None);
        assert_ne!(entries[1].slot, entries[2].slot);
    }
    #[test]
    fn selected_installation_matches_logical_running_application_without_duplicate_saved_chips() {
        let apps = [AppEntry {
            id: "flatpak.org.example.Editor".parse().unwrap(),
            logical: "org.example.Editor".into(),
            name: "Editor".into(),
            desktop_file_id: "editor.desktop".into(),
            icon: String::new(),
            categories: vec![],
            keywords: vec![],
            no_display: false,
            variants: 2,
        }];
        let variants = HashMap::from([(
            "org.example.Editor".into(),
            vec![
                (
                    "flatpak.org.example.Editor".into(),
                    "Flatpak".into(),
                    String::new(),
                ),
                (
                    "desktop-import.org.example.Editor".into(),
                    "System".into(),
                    String::new(),
                ),
            ],
        )]);
        assert!(matches_application(
            "desktop-import.org.example.Editor",
            "org.example.Editor",
            &apps,
            &variants
        ));
        assert!(matches_application(
            "flatpak.org.example.Editor",
            "desktop-import.org.example.Editor",
            &apps,
            &variants
        ));
        assert!(!matches_application(
            "flatpak.org.example.Editor",
            "org.example.Other",
            &apps,
            &variants
        ));
        let prefs = LauncherPreferences {
            favorites: [
                ("saved", "flatpak.org.example.Editor"),
                ("duplicate", "desktop-import.org.example.Editor"),
            ]
            .into_iter()
            .map(|(id, app)| Favorite {
                id: id.into(),
                label: "Work editor".into(),
                target: FavoriteTarget::App { app_id: app.into() },
            })
            .collect(),
            ..LauncherPreferences::default()
        };
        let entries = reconcile(
            crate::taskbar::entries(&[top(1, "org.example.Editor", true)]),
            &prefs,
            &apps,
            &variants,
            &HashMap::new(),
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].slot, RailSlot::Favorite("saved".into()));
        assert_eq!(entries[0].windows()[0].id, WindowId(1));
        assert_eq!(entries[0].label, "Work editor");
    }
}
