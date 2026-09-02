//! Recently launched applications, in the order a person reaches for them.
//!
//! This is the same algorithm as the COSMIC app-library recent-apps work (the
//! `record_recent_app` / `visible_apps` patch in `/home/dk/cosmic-recent-apps.patch`): a launch
//! moves that application to the front of a bounded list, duplicates collapse, and a missing
//! install is skipped rather than shown as a hole. Original cosmic-launcher has no such list;
//! the fork put it on the empty-search Home group. Home's search control is that group now, so
//! the list lives under the search field rather than inside a separate library window.
//!
//! Pure: ids in, ids out. Persistence is a `Settings1` `str-list`; drawing is the field screen.

use crate::app_search::AppEntry;

/// How many launches the strip remembers. Ten is the COSMIC fork's own cap: enough to scan,
/// not enough to become a second launcher.
pub const RECENT_APP_LIMIT: usize = 10;

/// Newest first. Empty is the honest cold-session state: there is nothing to promote yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentApps {
    ids: Vec<String>,
}

impl RecentApps {
    /// Rebuild from a stored newest-first list. Duplicates and empties are dropped, the cap is
    /// honoured, and the stored order is kept — this is a load, not a sequence of launches.
    pub fn from_ids(ids: impl IntoIterator<Item = impl Into<String>>) -> RecentApps {
        let mut recents = RecentApps::default();
        for id in ids {
            let id = id.into();
            if id.is_empty() || recents.ids.iter().any(|existing| existing == &id) {
                continue;
            }
            recents.ids.push(id);
            if recents.ids.len() == RECENT_APP_LIMIT {
                break;
            }
        }
        recents
    }

    /// A launch: this application is now the most recent. Identical to the COSMIC fork's
    /// `record_recent_app` — retain, insert at the front, truncate.
    pub fn record(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.ids.retain(|recent| recent != id);
        self.ids.insert(0, id.to_owned());
        self.ids.truncate(RECENT_APP_LIMIT);
    }

    /// Newest first.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// The installed, offerable applications these ids still name, in list order. An id the
    /// registry no longer has — uninstalled, or `NoDisplay` — is skipped, never shown as a
    /// blank chip.
    pub fn resolve<'a>(&self, apps: &'a [AppEntry]) -> Vec<&'a AppEntry> {
        self.ids
            .iter()
            .filter_map(|id| {
                apps.iter()
                    .find(|app| app.offerable() && app.id.as_str() == id)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_search::AppEntry;

    fn entry(id: &str, name: &str, no_display: bool) -> AppEntry {
        AppEntry {
            id: id.parse().unwrap(),
            name: name.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display,
            logical: String::new(),
            variants: 1,
        }
    }

    #[test]
    fn a_launch_moves_that_application_to_the_front_and_collapses_duplicates() {
        let mut recents = RecentApps::default();
        recents.record("org.legeos.Files");
        recents.record("org.legeos.Terminal");
        recents.record("org.legeos.Files");
        assert_eq!(recents.ids(), ["org.legeos.Files", "org.legeos.Terminal"]);
    }

    #[test]
    fn the_list_never_grows_past_the_cap() {
        let mut recents = RecentApps::default();
        for n in 0..20 {
            recents.record(&format!("org.legeos.App{n}"));
        }
        assert_eq!(recents.ids().len(), RECENT_APP_LIMIT);
        assert_eq!(recents.ids()[0], "org.legeos.App19");
        assert_eq!(recents.ids()[9], "org.legeos.App10");
    }

    #[test]
    fn a_stored_list_keeps_its_order_and_drops_holes() {
        let recents = RecentApps::from_ids([
            "org.legeos.Files",
            "org.legeos.Files",
            "",
            "org.legeos.Terminal",
        ]);
        assert_eq!(recents.ids(), ["org.legeos.Files", "org.legeos.Terminal"]);
    }

    #[test]
    fn resolve_skips_uninstalls_and_entries_that_must_not_be_offered() {
        let recents =
            RecentApps::from_ids(["org.legeos.Gone", "org.legeos.Hidden", "org.legeos.Files"]);
        let apps = [
            entry("org.legeos.Hidden", "Hidden", true),
            entry("org.legeos.Files", "Files", false),
        ];
        let resolved: Vec<&str> = recents
            .resolve(&apps)
            .iter()
            .map(|app| app.id.as_str())
            .collect();
        assert_eq!(resolved, ["org.legeos.Files"]);
    }
}
