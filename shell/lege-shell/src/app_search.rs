//! Finding an application among the ones installed: the matching and ranking behind the Home
//! Field's launcher and its search bar.
//!
//! Client-side on purpose. `AppIdentity1.ListApplications` already hands over the whole inventory
//! — name, icon, categories, keywords, and whether the entry is meant to be offered — and a few
//! hundred rows is nothing to filter in a frame. A `Search` method on the bus would put a policy
//! ("what does this query mean?") behind an interface, make every keystroke a round trip, and
//! leave the shell unable to show anything at all when sessiond is slow. Filtering data a service
//! has already provided is explicitly the shell's to do; deciding *what may be launched* is not,
//! and stays where it is.
//!
//! Everything here is pure: `&[AppEntry]` and a query in, an ordered `Vec<Match>` out. No I/O, no
//! bus, no `Ui`.

use lege_types::AppId;

/// One installed application as `AppIdentity1.ListApplications` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    pub id: AppId,
    /// What to call it. Never empty in practice; the id is shown if it is.
    pub name: String,
    /// The `.desktop` file this came from, for the icon lookup (`crate::icons::IconHints`).
    pub desktop_file_id: String,
    /// The `Icon=` value, empty when the application declared none.
    pub icon: String,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    /// `NoDisplay=true`: launchable, but never to be *offered*.
    pub no_display: bool,
}

impl AppEntry {
    /// The label a surface shows: the display name, falling back to the canonical id so a row is
    /// never blank.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            self.id.as_str()
        } else {
            &self.name
        }
    }

    /// Whether this entry belongs in a list a person is choosing from.
    ///
    /// `NoDisplay` is the desktop-entry spec's own "this exists so something else can find it":
    /// MIME shims, autostart shells, `*-url-handler` entries. They must stay installed and
    /// launchable — an association resolves to one — and must never appear in a launcher, which
    /// on a stock desktop is the difference between forty rows and four hundred.
    pub fn offerable(&self) -> bool {
        !self.no_display
    }
}

/// Why an entry matched, best first. The ordering of the variants *is* the ranking.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MatchKind {
    /// The query is the whole name: typing "files" should not put "Files Backup Helper" first.
    ExactName,
    /// The name starts with the query — the common case for someone typing the first letters.
    NamePrefix,
    /// A later word of the name starts with the query ("code" finding "Visual Studio Code").
    NameWordPrefix,
    /// The name contains the query somewhere inside a word.
    NameSubstring,
    /// A `Keywords=` term starts with the query. Below every name match: a keyword is the
    /// application's own suggestion, not what the person is looking at on screen.
    KeywordPrefix,
    KeywordSubstring,
    /// A `Categories=` entry matches ("network", "development").
    Category,
    /// The canonical id or desktop file id contains the query — the last resort, so that an
    /// application whose display name is nothing like its command is still reachable.
    Identifier,
}

/// One ranked result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match<'a> {
    pub entry: &'a AppEntry,
    pub kind: MatchKind,
}

/// Every offerable entry, in launcher order, when there is no query: alphabetical by label,
/// case-insensitively, so the list a person scans does not reorder under them.
pub fn all_offerable(entries: &[AppEntry]) -> Vec<Match<'_>> {
    let mut rows: Vec<Match<'_>> = entries
        .iter()
        .filter(|entry| entry.offerable())
        .map(|entry| Match {
            entry,
            kind: MatchKind::NamePrefix,
        })
        .collect();
    // Plain alphabetical, *not* [`sort_key`]: with no query there is nothing to rank, and the
    // shortest-name-first tiebreak that makes a ranked list read well would make an unranked one
    // look shuffled.
    rows.sort_by_key(|row| {
        (
            row.entry.label().to_lowercase(),
            row.entry.id.as_str().to_owned(),
        )
    });
    rows
}

/// The offerable entries matching `query`, best first.
///
/// An empty or whitespace-only query is not "match nothing" — it is "no filter", which is what a
/// launcher tile shows before anyone has typed. Matching is ASCII-case-insensitive on both sides.
pub fn search<'a>(entries: &'a [AppEntry], query: &str) -> Vec<Match<'a>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return all_offerable(entries);
    }
    let mut rows: Vec<Match<'a>> = entries
        .iter()
        .filter(|entry| entry.offerable())
        .filter_map(|entry| classify(entry, &needle).map(|kind| Match { entry, kind }))
        .collect();
    rows.sort_by_key(sort_key);
    rows
}

/// The single best match for `query`, if any — what Enter in the search bar launches.
pub fn best<'a>(entries: &'a [AppEntry], query: &str) -> Option<&'a AppEntry> {
    if query.trim().is_empty() {
        return None;
    }
    search(entries, query).into_iter().next().map(|m| m.entry)
}

/// Ties are broken by the shorter label and then alphabetically, so the order is total and a
/// redraw can never shuffle two equally good rows past each other.
fn sort_key(row: &Match<'_>) -> (MatchKind, usize, String, String) {
    let label = row.entry.label();
    (
        row.kind,
        label.chars().count(),
        label.to_lowercase(),
        row.entry.id.as_str().to_owned(),
    )
}

/// The best reason `entry` matches `needle` (already lower-cased and trimmed).
fn classify(entry: &AppEntry, needle: &str) -> Option<MatchKind> {
    let label = entry.label().to_lowercase();
    if label == needle {
        return Some(MatchKind::ExactName);
    }
    if label.starts_with(needle) {
        return Some(MatchKind::NamePrefix);
    }
    if word_prefix(&label, needle) {
        return Some(MatchKind::NameWordPrefix);
    }
    if label.contains(needle) {
        return Some(MatchKind::NameSubstring);
    }
    let mut keyword = None;
    for term in &entry.keywords {
        let term = term.to_lowercase();
        if term.starts_with(needle) {
            keyword = Some(MatchKind::KeywordPrefix);
            break;
        }
        if term.contains(needle) {
            keyword = Some(MatchKind::KeywordSubstring);
        }
    }
    if let Some(kind) = keyword {
        return Some(kind);
    }
    if entry
        .categories
        .iter()
        .any(|category| category.to_lowercase().contains(needle))
    {
        return Some(MatchKind::Category);
    }
    let identifier_hit = entry.id.as_str().to_lowercase().contains(needle)
        || entry.desktop_file_id.to_lowercase().contains(needle);
    identifier_hit.then_some(MatchKind::Identifier)
}

/// Whether any word of `haystack` after the first starts with `needle`. Words break on
/// whitespace and on the punctuation that runs through application names (`-`, `_`, `.`, `/`).
fn word_prefix(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | '('))
        .skip(1)
        .any(|word| word.starts_with(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.parse().expect("a valid test id"),
            name: name.to_owned(),
            desktop_file_id: format!("{}.desktop", name.to_lowercase()),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
        }
    }

    fn labels<'a>(rows: &[Match<'a>]) -> Vec<&'a str> {
        rows.iter().map(|row| row.entry.label()).collect()
    }

    fn catalogue() -> Vec<AppEntry> {
        let mut firefox = entry("desktop-import.firefox.desktop", "Firefox");
        firefox.keywords = vec!["web".into(), "browser".into(), "internet".into()];
        firefox.categories = vec!["Network".into(), "WebBrowser".into()];

        let mut code = entry("desktop-import.code.desktop", "Visual Studio Code");
        code.keywords = vec!["editor".into()];
        code.categories = vec!["Development".into()];

        let mut helper = entry("desktop-import.firefox-url.desktop", "Firefox URL Handler");
        helper.no_display = true;

        let mut terminal = entry("org.legeos.Terminal", "Terminal");
        terminal.keywords = vec!["shell".into(), "console".into()];

        vec![
            firefox,
            code,
            helper,
            terminal,
            entry("org.legeos.Files", "Files"),
            entry("desktop-import.file-roller.desktop", "Archive Manager"),
        ]
    }

    #[test]
    fn a_no_display_entry_is_never_offered_even_when_it_matches_perfectly() {
        let apps = catalogue();
        let rows = search(&apps, "Firefox URL Handler");
        assert!(rows.is_empty(), "{:?}", labels(&rows));
        assert!(!all_offerable(&apps).iter().any(|row| row.entry.no_display));
    }

    #[test]
    fn an_exact_name_wins_over_a_longer_name_that_also_starts_with_it() {
        let mut apps = catalogue();
        apps.push(entry("org.legeos.FilesBackup", "Files Backup Helper"));
        let rows = search(&apps, "files");
        assert_eq!(labels(&rows), ["Files", "Files Backup Helper"]);
        assert_eq!(rows[0].kind, MatchKind::ExactName);
    }

    #[test]
    fn a_later_word_of_the_name_is_findable_without_typing_the_first() {
        let apps = catalogue();
        let rows = search(&apps, "code");
        assert_eq!(labels(&rows), ["Visual Studio Code"]);
        assert_eq!(rows[0].kind, MatchKind::NameWordPrefix);
    }

    #[test]
    fn a_keyword_finds_an_application_whose_name_says_nothing_about_it() {
        let apps = catalogue();
        let rows = search(&apps, "browser");
        assert_eq!(labels(&rows), ["Firefox"]);
        assert_eq!(rows[0].kind, MatchKind::KeywordPrefix);
    }

    #[test]
    fn a_category_is_the_weakest_match_and_still_a_match() {
        let apps = catalogue();
        let rows = search(&apps, "development");
        assert_eq!(labels(&rows), ["Visual Studio Code"]);
        assert_eq!(rows[0].kind, MatchKind::Category);
    }

    #[test]
    fn a_name_match_always_outranks_a_keyword_match_for_the_same_query() {
        let mut apps = catalogue();
        let mut shell = entry("org.legeos.Shell", "Shell");
        shell.keywords = vec!["desktop".into()];
        apps.push(shell);
        // "shell" is Terminal's keyword and Shell's whole name.
        let rows = search(&apps, "shell");
        assert_eq!(labels(&rows), ["Shell", "Terminal"]);
        assert_eq!(rows[0].kind, MatchKind::ExactName);
        assert_eq!(rows[1].kind, MatchKind::KeywordPrefix);
    }

    #[test]
    fn an_identifier_match_is_the_last_resort_rather_than_no_match_at_all() {
        let apps = catalogue();
        let rows = search(&apps, "file-roller");
        assert_eq!(labels(&rows), ["Archive Manager"]);
        assert_eq!(rows[0].kind, MatchKind::Identifier);
    }

    #[test]
    fn an_empty_query_lists_everything_offerable_in_a_stable_alphabetical_order() {
        let apps = catalogue();
        let rows = all_offerable(&apps);
        assert_eq!(
            labels(&rows),
            [
                "Archive Manager",
                "Files",
                "Firefox",
                "Terminal",
                "Visual Studio Code",
            ]
        );
        assert_eq!(labels(&search(&apps, "   ")), labels(&rows));
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert_eq!(
            best(&catalogue(), "FIREFOX").map(AppEntry::label),
            Some("Firefox")
        );
    }

    #[test]
    fn the_best_match_is_what_enter_would_launch_and_an_empty_query_launches_nothing() {
        let apps = catalogue();
        assert_eq!(best(&apps, "fire").map(AppEntry::label), Some("Firefox"));
        assert_eq!(best(&apps, ""), None);
        assert_eq!(best(&apps, "nothing matches this"), None);
    }

    #[test]
    fn a_nameless_entry_shows_its_id_rather_than_an_empty_row() {
        let nameless = entry("org.legeos.Nameless", "");
        assert_eq!(nameless.label(), "org.legeos.Nameless");
    }

    #[test]
    fn ranking_is_a_total_order_so_a_redraw_cannot_reshuffle_equal_rows() {
        let apps = vec![
            entry("org.legeos.Alpha", "Same Name"),
            entry("org.legeos.Beta", "Same Name"),
        ];
        let first = search(&apps, "same");
        let second = search(&apps, "same");
        let ids: Vec<&str> = first.iter().map(|row| row.entry.id.as_str()).collect();
        assert_eq!(ids, ["org.legeos.Alpha", "org.legeos.Beta"]);
        assert_eq!(
            ids,
            second
                .iter()
                .map(|row| row.entry.id.as_str())
                .collect::<Vec<_>>()
        );
    }
}
