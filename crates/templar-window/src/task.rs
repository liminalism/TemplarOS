//! The one task model: which unit a window belongs to, and in what order.
//!
//! Four surfaces ask about windows — the Task Island, Taskfield, `Alt+Tab` and the launcher — and
//! each used to compute its own grouping and its own order, so they could disagree about what was
//! running. [`units`] is the single answer.
//!
//! It is a pure function over [`ShellToplevel`] rather than a method on [`crate::WindowService`] on
//! purpose: the compositor holds those records and the shell receives exactly the same ones over
//! `templar_shell_v1`, so both sides run *this* function on the same bytes instead of keeping two
//! models in step. `WindowService::task_units` is one line over it.
//!
//! Recency is not recomputed here either. [`ShellToplevel::mru_rank`] is the service's activation
//! order (`WindowRecord::last_activated`, bumped in `activate`), so ordering a unit's windows by it
//! *is* ordering them by activation — `Alt+Tab`, the fan, the launcher's Enter and a chip's
//! unshelve target cannot invent an order of their own.

use std::collections::BTreeMap;

use crate::{GroupId, ShellToplevel, WindowId};
use templar_types::AppId;

/// How many windows a unit may have before its chip stops offering a fan and hands over to
/// Taskfield. A linear strip is intrinsically bad at thirty windows; this is where it admits it.
pub const FAN_MAX: usize = 4;

/// What one island chip, one Taskfield section and one `` Alt+` `` scope name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TaskKey {
    /// An explicit window group. The user made it, so it outranks application identity: a grouped
    /// window is in its group's unit and in no application's.
    Group(GroupId),
    App(AppId),
    /// A window whose identity is not verified: **one unit per window, never merged**. A claimed
    /// `app_id` must not join a trusted application's chip (ARCHITECTURE.md's identity invariant).
    Unknown(WindowId),
}

impl TaskKey {
    /// The wire and settings spelling — `open_taskfield`'s `key`, and the string the bar has always
    /// filed a chip under ([`crate::grouping_key`]), so the two cannot drift.
    #[must_use]
    pub fn wire(&self) -> String {
        match self {
            Self::Group(group) => format!("group:{}", group.0),
            Self::App(app) => app.to_string(),
            Self::Unknown(window) => format!("unknown-window-{}", window.0),
        }
    }

    /// The inverse of [`Self::wire`]. `None` for a string that names nothing this desktop can have.
    #[must_use]
    pub fn from_wire(key: &str) -> Option<Self> {
        if let Some(id) = key.strip_prefix("group:") {
            return id.parse().ok().map(|id| Self::Group(GroupId(id)));
        }
        if let Some(id) = key.strip_prefix("unknown-window-") {
            return id.parse().ok().map(|id| Self::Unknown(WindowId(id)));
        }
        key.parse().ok().map(Self::App)
    }
}

/// One unit of work: an explicit group, or an application with its loose windows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskUnit {
    pub key: TaskKey,
    /// The raw name: the group's (possibly empty), the application id, or an unidentified window's
    /// title. Deliberately not prettified — the shell has the icon cache and the display-name rules
    /// (`field_model::short_name`, `group_title`); this crate has neither and must not guess.
    pub label: String,
    /// The group's accent (0x00RRGGBB); `None` for an application.
    pub accent: Option<u32>,
    /// Every window of the unit, most recently activated first, shelved ones included.
    pub windows: Vec<WindowId>,
    /// Which of [`Self::windows`] are parked on the shelf, in the same order.
    pub shelved: Vec<WindowId>,
}

impl TaskUnit {
    /// How many of the unit's windows are on screen. The count the chip draws.
    #[must_use]
    pub fn visible(&self) -> usize {
        self.windows.len() - self.shelved.len()
    }

    /// Whether nothing of this unit is on screen — what makes a chip read as fully dimmed.
    #[must_use]
    pub fn all_shelved(&self) -> bool {
        !self.windows.is_empty() && self.shelved.len() == self.windows.len()
    }

    /// The unit's on-screen windows, most recently activated first.
    pub fn visible_windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.windows
            .iter()
            .copied()
            .filter(|id| !self.shelved.contains(id))
    }
}

/// What a click or a dwell on a chip does, by how much there is to show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChipAction {
    /// Nothing to choose between: activate (unshelving first if that is all there is).
    Direct,
    /// Few enough to fan out of the chip as preview cards.
    Fan,
    /// Too many for a strip: hand over to Taskfield, filtered to this unit.
    Taskfield,
}

/// [`ChipAction`] for a unit, by its visible window count.
#[must_use]
pub fn chip_action(unit: &TaskUnit) -> ChipAction {
    match unit.visible() {
        0 | 1 => ChipAction::Direct,
        n if n <= FAN_MAX => ChipAction::Fan,
        _ => ChipAction::Taskfield,
    }
}

/// Every task unit: explicit groups first in group-id order, then applications and unidentified
/// windows in **first-seen order** — the order they appear in `toplevels`, which the service sorts
/// by [`WindowId`], so a unit keeps its position for as long as it exists and a new one lands at
/// the end. The island's chips are spatial memory; activating must never make them swim.
#[must_use]
pub fn units(toplevels: &[ShellToplevel]) -> Vec<TaskUnit> {
    let mut groups: BTreeMap<GroupId, Vec<&ShellToplevel>> = BTreeMap::new();
    // First-seen order is the point, so this is a list rather than a map; a desktop holds tens of
    // windows, not thousands.
    // ponytail: linear lookup per window. A parallel index is worth it above a few hundred windows.
    let mut loose: Vec<(TaskKey, Vec<&ShellToplevel>)> = Vec::new();

    for toplevel in toplevels {
        if let Some(info) = &toplevel.group {
            groups
                .entry(GroupId(info.group_id))
                .or_default()
                .push(toplevel);
            continue;
        }
        let key = loose_key(toplevel);
        match loose.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, members)) => members.push(toplevel),
            None => loose.push((key, vec![toplevel])),
        }
    }

    let mut out = Vec::with_capacity(groups.len() + loose.len());
    for (id, members) in groups {
        let info = members[0]
            .group
            .as_ref()
            .expect("bucketed by the group it carries");
        out.push(unit(
            TaskKey::Group(id),
            info.name.clone(),
            Some(info.accent),
            members,
        ));
    }
    for (key, members) in loose {
        let label = match &key {
            TaskKey::App(app) => app.to_string(),
            _ => members[0].title.clone(),
        };
        out.push(unit(key, label, None, members));
    }
    out
}

/// The unit `current` belongs to, if any.
#[must_use]
pub fn unit_of(units: &[TaskUnit], current: WindowId) -> Option<&TaskUnit> {
    units.iter().find(|unit| unit.windows.contains(&current))
}

/// Every window of `current`'s unit, parked ones included, most recently activated first: the
/// group's members when it is grouped, the application's windows otherwise. `` Alt+` `` walks
/// this, and like `Alt+Tab` it lists what is open rather than only what is on screen — the chord
/// activates on release, which unshelves a parked window. The fan reads
/// [`TaskUnit::visible_windows`] instead.
#[must_use]
pub fn within_unit(units: &[TaskUnit], current: WindowId) -> Vec<WindowId> {
    unit_of(units, current).map_or_else(Vec::new, |unit| unit.windows.clone())
}

/// Whether a window answers a Taskfield query: an empty query matches everything, and a non-empty
/// one is matched case-insensitively against the window's title and its unit's label (the
/// application id or the group name). Pure, so search keeps working with the shell dead.
#[must_use]
pub fn matches(unit: &TaskUnit, toplevel: &ShellToplevel, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    toplevel.title.to_lowercase().contains(&query) || unit.label.to_lowercase().contains(&query)
}

fn loose_key(toplevel: &ShellToplevel) -> TaskKey {
    match (&toplevel.canonical_app_id, toplevel.verified_identity) {
        (Some(app), true) => TaskKey::App(app.clone()),
        _ => TaskKey::Unknown(toplevel.window_id),
    }
}

fn unit(
    key: TaskKey,
    label: String,
    accent: Option<u32>,
    mut members: Vec<&ShellToplevel>,
) -> TaskUnit {
    members.sort_by_key(|member| (member.mru_rank, member.window_id));
    TaskUnit {
        key,
        label,
        accent,
        windows: members.iter().map(|member| member.window_id).collect(),
        shelved: members
            .iter()
            .filter(|member| member.shelved)
            .map(|member| member.window_id)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupInfo, Layer, Rect};

    /// One toplevel as the shell receives it. `app` `None` is an unverified window.
    fn top(id: u64, app: Option<&str>, mru: u32) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: app.map(|app| app.parse().expect("a valid application id")),
            claimed_app_id: None,
            instance_id: None,
            title: format!("window {id}"),
            verified_identity: app.is_some(),
            activated: false,
            mru_rank: mru,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
            group: None,
        }
    }

    fn grouped(mut toplevel: ShellToplevel, group_id: u64) -> ShellToplevel {
        toplevel.group = Some(GroupInfo {
            group_id,
            name: format!("group {group_id}"),
            accent: crate::accent_for(GroupId(group_id)),
        });
        toplevel
    }

    fn keys(units: &[TaskUnit]) -> Vec<String> {
        units.iter().map(|unit| unit.key.wire()).collect()
    }

    #[test]
    fn units_are_groups_first_then_apps_in_stable_order() {
        let toplevels = vec![
            top(1, Some("org.templaros.Files"), 3),
            grouped(top(2, Some("org.templaros.Term"), 1), 7),
            top(3, Some("org.templaros.Files"), 2),
            grouped(top(4, Some("org.templaros.Text"), 0), 2),
            top(5, Some("org.templaros.Text"), 4),
        ];
        assert_eq!(
            keys(&units(&toplevels)),
            vec![
                "group:2",
                "group:7",
                "org.templaros.Files",
                "org.templaros.Text"
            ],
            "groups in group-id order, then applications in the order their first window appears"
        );
    }

    #[test]
    fn a_unit_orders_its_windows_by_activation_not_by_id() {
        let toplevels = vec![
            top(1, Some("org.templaros.Files"), 2),
            top(2, Some("org.templaros.Files"), 0),
            top(3, Some("org.templaros.Files"), 1),
        ];
        let units = units(&toplevels);
        assert_eq!(
            units[0].windows,
            vec![WindowId(2), WindowId(3), WindowId(1)]
        );
    }

    #[test]
    fn activating_never_reorders_units() {
        let toplevels = vec![
            top(1, Some("org.templaros.Files"), 0),
            top(2, Some("org.templaros.Term"), 1),
            top(3, Some("org.templaros.Text"), 2),
        ];
        let before = keys(&units(&toplevels));

        // Every window takes its turn at being the most recently activated one.
        for newest in 0..toplevels.len() {
            let mut shuffled = toplevels.clone();
            for (index, toplevel) in shuffled.iter_mut().enumerate() {
                toplevel.mru_rank =
                    u32::try_from((index + toplevels.len() - newest) % toplevels.len())
                        .expect("three windows");
            }
            assert_eq!(keys(&units(&shuffled)), before, "chips must not swim");
        }
    }

    #[test]
    fn unverified_identity_is_never_merged() {
        let mut claimed = top(2, Some("org.templaros.Files"), 1);
        claimed.verified_identity = false;
        let toplevels = vec![top(1, Some("org.templaros.Files"), 0), claimed];

        let units = units(&toplevels);
        assert_eq!(
            keys(&units),
            vec!["org.templaros.Files", "unknown-window-2"],
            "a claimed app id must not join the trusted application's chip"
        );
        assert_eq!(units[1].label, "window 2", "an unknown unit is its title");
    }

    #[test]
    fn chip_action_hands_over_above_four() {
        let app = "org.templaros.Files";
        let counts = |n: u64| {
            let toplevels: Vec<ShellToplevel> = (1..=n)
                .map(|id| top(id, Some(app), u32::try_from(id).expect("small")))
                .collect();
            chip_action(&units(&toplevels)[0])
        };
        assert_eq!(counts(1), ChipAction::Direct);
        assert_eq!(counts(2), ChipAction::Fan);
        assert_eq!(counts(4), ChipAction::Fan);
        assert_eq!(counts(5), ChipAction::Taskfield);
    }

    #[test]
    fn shelved_windows_count_against_the_chip_but_stay_in_the_unit() {
        let mut parked = top(2, Some("org.templaros.Files"), 1);
        parked.shelved = true;
        let mut also_parked = top(3, Some("org.templaros.Files"), 2);
        also_parked.shelved = true;
        let mixed = units(&[
            top(1, Some("org.templaros.Files"), 0),
            parked.clone(),
            also_parked,
        ]);

        assert_eq!(mixed[0].windows.len(), 3);
        assert_eq!(mixed[0].visible(), 1);
        assert_eq!(chip_action(&mixed[0]), ChipAction::Direct);
        assert!(!mixed[0].all_shelved());

        let all = units(&[parked]);
        assert!(all[0].all_shelved());
        assert_eq!(chip_action(&all[0]), ChipAction::Direct);
    }

    #[test]
    fn within_unit_is_the_group_when_grouped_and_the_application_otherwise() {
        let toplevels = vec![
            grouped(top(1, Some("org.templaros.Files"), 1), 1),
            grouped(top(2, Some("org.templaros.Term"), 0), 1),
            top(3, Some("org.templaros.Text"), 2),
            top(4, Some("org.templaros.Text"), 3),
        ];
        let units = units(&toplevels);

        assert_eq!(
            within_unit(&units, WindowId(1)),
            vec![WindowId(2), WindowId(1)],
            "a grouped window's unit is its group, across applications"
        );
        assert_eq!(
            within_unit(&units, WindowId(3)),
            vec![WindowId(3), WindowId(4)],
            "a loose window's unit is its application"
        );
        assert!(within_unit(&units, WindowId(99)).is_empty());
    }

    #[test]
    fn within_unit_keeps_parked_windows_in_their_recency_position() {
        let mut parked = top(2, Some("org.templaros.Files"), 0);
        parked.shelved = true;
        let units = units(&[top(1, Some("org.templaros.Files"), 1), parked]);
        assert_eq!(
            within_unit(&units, WindowId(1)),
            vec![WindowId(2), WindowId(1)]
        );
    }

    #[test]
    fn a_query_matches_a_title_or_the_unit_label() {
        let toplevel = top(1, Some("org.templaros.Files"), 0);
        let units = units(std::slice::from_ref(&toplevel));
        let unit = &units[0];

        assert!(matches(unit, &toplevel, ""), "no query matches everything");
        assert!(
            matches(unit, &toplevel, "WINDOW"),
            "title, case-insensitive"
        );
        assert!(matches(unit, &toplevel, "files"), "the unit's label");
        assert!(!matches(unit, &toplevel, "blender"));
    }

    #[test]
    fn every_key_survives_the_wire() {
        for key in [
            TaskKey::Group(GroupId(4)),
            TaskKey::App("org.templaros.Files".parse().expect("valid")),
            TaskKey::Unknown(WindowId(12)),
        ] {
            assert_eq!(
                TaskKey::from_wire(&key.wire()),
                Some(key.clone()),
                "{key:?}"
            );
        }
        assert_eq!(TaskKey::from_wire("group:x"), None);
        assert_eq!(TaskKey::from_wire(""), None);
    }
}
