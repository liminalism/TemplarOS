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

use templar_types::AppId;

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
    /// The logical application this is a copy of (`AppIdentity1.ListLogicalApps`), or empty when
    /// the registry does not group applications. One Firefox in a search, however many copies of
    /// it are installed.
    pub logical: String,
    /// How many copies of that logical application are installed. One is the ordinary case, and
    /// the only case worth saying anything about is more than one.
    pub variants: u32,
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
    /// The query's letters appear in the name in order, starting at a word boundary — the
    /// abbreviation and the typo ("vsc" for Visual Studio Code, "gimq" for GIMP). Gap-penalised,
    /// so a match that runs together beats one scattered across the name; [`Match::score`] is the
    /// order *within* this tier.
    NameFuzzy,
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
    /// How good the match is *within* [`Match::kind`], higher first. Only [`MatchKind::NameFuzzy`]
    /// has anything to say here; every other tier is already an exact statement about the match
    /// and scores 0.
    pub score: u32,
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
            score: 0,
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
///
/// `recents` is the newest-first list of application ids from [`crate::recents::RecentApps`]. It
/// never moves a row across a tier — the thing you named still beats the thing you last opened —
/// but within one tier the application you reached for most recently comes first, which is what
/// makes the second use of a launcher shorter than the first.
pub fn search<'a>(entries: &'a [AppEntry], query: &str, recents: &[String]) -> Vec<Match<'a>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return all_offerable(entries);
    }
    let mut rows: Vec<Match<'a>> = entries
        .iter()
        .filter(|entry| entry.offerable())
        .filter_map(|entry| {
            classify(entry, &needle).map(|(kind, score)| Match { entry, kind, score })
        })
        .collect();
    rows.sort_by_key(|row| sort_key(row, recents));
    rows
}

/// The single best match for `query`, if any — what Enter in the search bar launches.
pub fn best<'a>(entries: &'a [AppEntry], query: &str, recents: &[String]) -> Option<&'a AppEntry> {
    if query.trim().is_empty() {
        return None;
    }
    search(entries, query, recents)
        .into_iter()
        .next()
        .map(|m| m.entry)
}

/// How recently this application was launched, as a rank: 0 is the most recent, and everything
/// the list does not name sorts after everything it does.
fn recency(entry: &AppEntry, recents: &[String]) -> usize {
    recents
        .iter()
        .position(|id| id == entry.id.as_str())
        .unwrap_or(usize::MAX)
}

/// Ties are broken by recency, then by the fuzzy score, then by the shorter label and then
/// alphabetically, so the order is total and a redraw can never shuffle two equally good rows
/// past each other.
fn sort_key(
    row: &Match<'_>,
    recents: &[String],
) -> (
    MatchKind,
    usize,
    std::cmp::Reverse<u32>,
    usize,
    String,
    String,
) {
    let label = row.entry.label();
    (
        row.kind,
        recency(row.entry, recents),
        std::cmp::Reverse(row.score),
        label.chars().count(),
        label.to_lowercase(),
        row.entry.id.as_str().to_owned(),
    )
}

/// The best reason `entry` matches `needle` (already lower-cased and trimmed), and the score
/// that orders it within that reason.
fn classify(entry: &AppEntry, needle: &str) -> Option<(MatchKind, u32)> {
    let label = entry.label().to_lowercase();
    if label == needle {
        return Some((MatchKind::ExactName, 0));
    }
    if label.starts_with(needle) {
        return Some((MatchKind::NamePrefix, 0));
    }
    if word_prefix(&label, needle) {
        return Some((MatchKind::NameWordPrefix, 0));
    }
    if label.contains(needle) {
        return Some((MatchKind::NameSubstring, 0));
    }
    if let Some(score) = fuzzy(&label, needle) {
        return Some((MatchKind::NameFuzzy, score));
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
        return Some((kind, 0));
    }
    if entry
        .categories
        .iter()
        .any(|category| category.to_lowercase().contains(needle))
    {
        return Some((MatchKind::Category, 0));
    }
    let identifier_hit = entry.id.as_str().to_lowercase().contains(needle)
        || entry.desktop_file_id.to_lowercase().contains(needle);
    identifier_hit.then_some((MatchKind::Identifier, 0))
}

/// How well `needle`'s letters read as an abbreviation of `label`, or `None` when they are not
/// all there in order or the first one lands mid-word.
///
/// This is the tier pop-launcher reaches for `strsim::jaro_winkler` to fill, written directly
/// because the shell has no string-similarity dependency and this is fifteen lines. The word
/// boundary requirement is what keeps it from being a wildcard: without it every query with
/// common letters matches half the menu.
///
/// ponytail: leftmost-greedy, not the optimal alignment — "aa" scores against the first `a` it
/// finds rather than the pair that would score best. Upgrade to a small DP table if a real
/// catalogue shows a wrong winner; a few hundred entries would still fit in a frame.
fn fuzzy(label: &str, needle: &str) -> Option<u32> {
    /// A run of adjacent letters is what an abbreviation looks like, so each further one is
    /// worth more than the last.
    const ADJACENT: i32 = 4;
    /// Landing on the start of a word is the other thing an abbreviation does.
    const BOUNDARY: i32 = 6;
    /// And a letter found in the middle of a word, after a gap, is worth almost nothing — minus
    /// how far it had to look, bounded so one long skip cannot outweigh every other letter.
    const GAP_LIMIT: i32 = 8;

    let mut score: i32 = 0;
    let mut run: i32 = 0;
    let mut previous_end: Option<usize> = None;
    for want in needle.chars() {
        let from = previous_end.unwrap_or(0);
        let at = from + label[from..].find(want)?;
        let boundary = at == 0 || label[..at].chars().next_back().is_some_and(is_word_break);
        match previous_end {
            // The first letter has to start a word: "vsc" abbreviates "Visual Studio Code", and
            // "sc" does not abbreviate anything just because those letters occur in it.
            None if !boundary => return None,
            None => score += BOUNDARY,
            Some(end) if end == at => {
                run += 1;
                score += ADJACENT + run;
            }
            Some(end) => {
                run = 0;
                let gap = i32::try_from(at - end).unwrap_or(GAP_LIMIT).min(GAP_LIMIT);
                score += if boundary { BOUNDARY } else { 1 } - gap;
            }
        }
        previous_end = Some(at + want.len_utf8());
    }
    u32::try_from(score.max(0)).ok()
}

/// Where one word of an application name ends: whitespace, and the punctuation that runs through
/// them. One predicate, so [`word_prefix`] and [`fuzzy`] can never disagree about where a word
/// starts.
fn is_word_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | '(')
}

/// Whether any word of `haystack` after the first starts with `needle`. Words break on
/// whitespace and on the punctuation that runs through application names (`-`, `_`, `.`, `/`).
fn word_prefix(haystack: &str, needle: &str) -> bool {
    haystack
        .split(is_word_break)
        .skip(1)
        .any(|word| word.starts_with(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cold session: nothing has been launched yet, so the ranking is the match alone.
    const NONE: &[String] = &[];

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.parse().expect("a valid test id"),
            name: name.to_owned(),
            desktop_file_id: format!("{}.desktop", name.to_lowercase()),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
            logical: String::new(),
            variants: 1,
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

        let mut terminal = entry("org.templaros.Terminal", "Terminal");
        terminal.keywords = vec!["shell".into(), "console".into()];

        vec![
            firefox,
            code,
            helper,
            terminal,
            entry("org.templaros.Files", "Files"),
            entry("desktop-import.file-roller.desktop", "Archive Manager"),
        ]
    }

    #[test]
    fn a_no_display_entry_is_never_offered_even_when_it_matches_perfectly() {
        let apps = catalogue();
        let rows = search(&apps, "Firefox URL Handler", NONE);
        assert!(rows.is_empty(), "{:?}", labels(&rows));
        assert!(!all_offerable(&apps).iter().any(|row| row.entry.no_display));
    }

    #[test]
    fn an_exact_name_wins_over_a_longer_name_that_also_starts_with_it() {
        let mut apps = catalogue();
        apps.push(entry("org.templaros.FilesBackup", "Files Backup Helper"));
        let rows = search(&apps, "files", NONE);
        assert_eq!(labels(&rows), ["Files", "Files Backup Helper"]);
        assert_eq!(rows[0].kind, MatchKind::ExactName);
    }

    #[test]
    fn a_later_word_of_the_name_is_findable_without_typing_the_first() {
        let apps = catalogue();
        let rows = search(&apps, "code", NONE);
        assert_eq!(labels(&rows), ["Visual Studio Code"]);
        assert_eq!(rows[0].kind, MatchKind::NameWordPrefix);
    }

    #[test]
    fn a_keyword_finds_an_application_whose_name_says_nothing_about_it() {
        let apps = catalogue();
        let rows = search(&apps, "browser", NONE);
        assert_eq!(labels(&rows), ["Firefox"]);
        assert_eq!(rows[0].kind, MatchKind::KeywordPrefix);
    }

    #[test]
    fn a_category_is_the_weakest_match_and_still_a_match() {
        let apps = catalogue();
        let rows = search(&apps, "development", NONE);
        assert_eq!(labels(&rows), ["Visual Studio Code"]);
        assert_eq!(rows[0].kind, MatchKind::Category);
    }

    #[test]
    fn a_name_match_always_outranks_a_keyword_match_for_the_same_query() {
        let mut apps = catalogue();
        let mut shell = entry("org.templaros.Shell", "Shell");
        shell.keywords = vec!["desktop".into()];
        apps.push(shell);
        // "shell" is Terminal's keyword and Shell's whole name.
        let rows = search(&apps, "shell", NONE);
        assert_eq!(labels(&rows), ["Shell", "Terminal"]);
        assert_eq!(rows[0].kind, MatchKind::ExactName);
        assert_eq!(rows[1].kind, MatchKind::KeywordPrefix);
    }

    #[test]
    fn an_identifier_match_is_the_last_resort_rather_than_no_match_at_all() {
        let apps = catalogue();
        let rows = search(&apps, "file-roller", NONE);
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
        assert_eq!(labels(&search(&apps, "   ", NONE)), labels(&rows));
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert_eq!(
            best(&catalogue(), "FIREFOX", NONE).map(AppEntry::label),
            Some("Firefox")
        );
    }

    #[test]
    fn the_best_match_is_what_enter_would_launch_and_an_empty_query_launches_nothing() {
        let apps = catalogue();
        assert_eq!(
            best(&apps, "fire", NONE).map(AppEntry::label),
            Some("Firefox")
        );
        assert_eq!(best(&apps, "", NONE), None);
        assert_eq!(best(&apps, "nothing matches this", NONE), None);
    }

    #[test]
    fn a_nameless_entry_shows_its_id_rather_than_an_empty_row() {
        let nameless = entry("org.templaros.Nameless", "");
        assert_eq!(nameless.label(), "org.templaros.Nameless");
    }

    /// The abbreviation tier: the letters in order, starting a word. This is the row a launcher
    /// lives or dies on — nobody types "visual studio code".
    #[test]
    fn an_abbreviation_finds_an_application_no_other_tier_would() {
        let apps = catalogue();
        let rows = search(&apps, "vsc", NONE);
        assert_eq!(labels(&rows), ["Visual Studio Code"]);
        assert_eq!(rows[0].kind, MatchKind::NameFuzzy);
    }

    /// And it is a tier, not a wildcard: letters that are all present but start mid-word are not
    /// an abbreviation of anything, or every query would match half the menu.
    #[test]
    fn scattered_letters_that_do_not_start_a_word_are_not_an_abbreviation() {
        // "isu" is inside "Visual" and in order. It may still reach the row by the last-resort
        // identifier tier, but it is not what this one is for.
        assert!(
            !search(&catalogue(), "isu", NONE)
                .iter()
                .any(|row| row.kind == MatchKind::NameFuzzy)
        );
    }

    /// Within the tier, the abbreviation whose letters start words beats the one that had to
    /// reach across a name to find them.
    #[test]
    fn a_tighter_abbreviation_outscores_a_more_scattered_one() {
        let apps = vec![
            entry("org.templaros.Code", "Visual Studio Code"),
            entry("org.templaros.Cat", "Very Small Cat"),
        ];
        let rows = search(&apps, "vsc", NONE);
        assert!(
            rows.iter().all(|row| row.kind == MatchKind::NameFuzzy),
            "{rows:?}"
        );
        assert_eq!(labels(&rows), ["Very Small Cat", "Visual Studio Code"]);
        assert!(rows[0].score > rows[1].score, "{rows:?}");
    }

    /// The recency boost: the one you last opened comes first among rows that matched equally
    /// well. The second use of a launcher is meant to be shorter than the first.
    #[test]
    fn the_application_you_last_opened_comes_first_among_equal_matches() {
        let apps = vec![
            entry("org.templaros.Notes", "Note Taker"),
            entry("org.templaros.Notify", "Notification Log"),
        ];
        assert_eq!(
            labels(&search(&apps, "not", NONE)),
            ["Note Taker", "Notification Log"],
            "the shorter label wins when nothing has been launched"
        );
        let recents = vec!["org.templaros.Notify".to_owned()];
        assert_eq!(
            labels(&search(&apps, "not", &recents)),
            ["Notification Log", "Note Taker"]
        );
    }

    /// But it is a boost inside a tier, never across one: the thing you named still beats the
    /// thing you last opened.
    #[test]
    fn recency_never_lifts_a_weaker_match_over_a_stronger_one() {
        let apps = catalogue();
        let recents = vec!["desktop-import.code.desktop".to_owned()];
        // "code" is Visual Studio Code's own word and only a category-ish miss for the rest.
        let rows = search(&apps, "fire", &recents);
        assert_eq!(labels(&rows), ["Firefox"], "{recents:?}");
        let rows = search(&apps, "term", &recents);
        assert_eq!(labels(&rows), ["Terminal"]);
    }

    #[test]
    fn ranking_is_a_total_order_so_a_redraw_cannot_reshuffle_equal_rows() {
        let apps = vec![
            entry("org.templaros.Alpha", "Same Name"),
            entry("org.templaros.Beta", "Same Name"),
        ];
        let first = search(&apps, "same", NONE);
        let second = search(&apps, "same", NONE);
        let ids: Vec<&str> = first.iter().map(|row| row.entry.id.as_str()).collect();
        assert_eq!(ids, ["org.templaros.Alpha", "org.templaros.Beta"]);
        assert_eq!(
            ids,
            second
                .iter()
                .map(|row| row.entry.id.as_str())
                .collect::<Vec<_>>()
        );
    }
}
