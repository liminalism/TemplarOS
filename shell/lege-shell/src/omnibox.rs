//! The two search controls of the shell, and what each one answers.
//!
//! The bar at the top of bare Home is the application launcher and nothing else: typing there
//! names an application to open. Files are not looked for from Home — a file search belongs to
//! the Files object, scoped to the location it is showing, and the wider search of everything
//! indexed is the job of a real search tool (the plan integrates fsearch for that). Projects are
//! tiles and actions have the context menu and the cheat sheet, so neither is a search result.
//!
//! The shell once federated every source into one list, and a query for a browser came back as
//! six unrelated files with the application somewhere among them. One control, one kind of
//! answer, is the rule now.
//!
//! Three rules make the file list trustworthy rather than merely full:
//!
//! 1. **Match strength first, source second.** `app_search::MatchKind` is the universal measure:
//!    an exact name beats a substring. Only when two rows match equally well does the source
//!    (a directory before a file) break the tie.
//! 2. **Every source is capped before the merge**, so a directory with a thousand matching files
//!    cannot crowd out the three folders that also matched.
//! 3. **A late answer never reorders what is on screen.** File search is asynchronous; a result
//!    whose query is not the one being asked is not shown at all, and its absence says "still
//!    searching" rather than "nothing found".
//!
//! Pure: sources in, rows out. No D-Bus, no `Ui`, no clock.

use std::path::{Path, PathBuf};

use lege_types::AppId;

use crate::app_search::{AppEntry, MatchKind};
use crate::file_browser::BrowseEntry;
use crate::files_surface::FilesLocation;

/// What one row stands for, and everything acting on it needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hit {
    App { id: AppId },
    Directory { path: PathBuf },
    File { path: PathBuf },
}

impl Hit {
    /// Which source a hit came from, as the tiebreak between two equally good matches: a
    /// directory before the files inside it, so a folder is never buried under its own contents.
    fn source_order(&self) -> u8 {
        match self {
            Self::App { .. } => 0,
            Self::Directory { .. } => 1,
            Self::File { .. } => 2,
        }
    }
}

/// What a row carries beyond a name and a second line.
///
/// One field rather than three optional ones, because the three cases are exclusive: a row is an
/// application in the launcher's list, a file in the search panel, or the panel's own last row,
/// which is not a result at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowExtra {
    /// An application in the launcher's dropdown. Nothing more to say than its name and id.
    None,
    /// A file or folder in the search panel, with the two facts the panel shows in its own
    /// columns: how big it is and when it last changed.
    File { size_bytes: u64, mtime_ms: i64 },
    /// "Search everywhere with FSearch": the panel's last row, which hands the question to the
    /// `file-search` provider instead of answering it.
    Everywhere,
}

/// One row of the dropdown, or of the search panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub hit: Hit,
    pub label: String,
    /// The second line: a path, a project's state, what an action does.
    pub detail: String,
    pub kind: MatchKind,
    /// What else this row carries — and, taken across the list, which of the two surfaces it is
    /// (see [`is_panel`]).
    pub extra: RowExtra,
}

/// The application id the "Search everywhere" row carries.
///
/// Not an installed application, and deliberately so: *which* program searches everywhere is a
/// `Providers1` choice (the `file-search` role), and this module is pure — it has no bus to ask
/// on. The row names the intent, the host resolves it to the selected provider and hands over
/// the query. A reserved id rather than a new `Hit` variant so every existing caller keeps
/// compiling and treating it as what it is: something to launch.
pub const SEARCH_EVERYWHERE: &str = "legeos.search-everywhere";

/// What the last row says. The provider is named because a person choosing to leave one search
/// for another deserves to know which program they are about to be handed to.
const SEARCH_EVERYWHERE_LABEL: &str = "Search everywhere with FSearch";

/// Whether this list is the Files object's search, which is shown as a panel rather than as a
/// dropdown under the field (plan §4.2): its rows carry three facts each, and a dropdown row
/// has room for one.
pub fn is_panel(rows: &[Row]) -> bool {
    rows.iter().any(|row| row.extra != RowExtra::None)
}

/// Where the question is being asked from. Derived from the surface, never chosen by the user —
/// there is no mode to be in and no mode to get stuck in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Bare Home: the application launcher. Applications and nothing else.
    Applications,
    /// Inside the expanded Files object: the files and folders of this location.
    Location(FilesLocation),
}

/// The last file search that completed, and the query it answered.
#[derive(Clone, Copy, Debug)]
pub struct FileResults<'a> {
    pub query: &'a str,
    pub entries: &'a [BrowseEntry],
}

/// Everything the dropdown can see. All of it is already in the shell's hands: this is
/// composition, not a new fetch, and deliberately not a server-side search interface. Which
/// source answers is the scope's decision, not the query's.
#[derive(Clone, Copy, Debug)]
pub struct Sources<'a> {
    pub apps: &'a [AppEntry],
    /// `None` while nothing has come back yet, which is a different thing from a search that
    /// found nothing — see [`file_state`].
    pub files: Option<FileResults<'a>>,
    /// The user's home directory, so a query beginning `~/` names somewhere real.
    pub home: Option<&'a Path>,
}

/// How many rows each file source may contribute before the merge.
///
/// Caps before the merge, not after: capping the merged list would let one prolific source
/// silently push another out entirely, and a search that stops showing folders the moment a
/// directory has many files is a search you stop trusting.
const DIRECTORY_CAP: usize = 3;
const FILE_CAP: usize = 9;
/// And the whole list is a dropdown, not a page.
pub const TOTAL_CAP: usize = 12;

/// What the file source has to say right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileState {
    /// Nothing has been asked yet, so there is nothing to say.
    Idle,
    /// The answer on hand is for a different question. Says "still searching" — never "nothing
    /// found", which would be a lie that flickers into truth a moment later.
    Searching,
    /// The answer on hand is for this question.
    Ready,
}

/// Whether the file results in hand answer `query`.
pub fn file_state(files: Option<FileResults<'_>>, query: &str) -> FileState {
    let needle = query.trim();
    if needle.is_empty() {
        return FileState::Idle;
    }
    // An answer for another question and no answer at all are the same thing to the person
    // waiting: neither says anything about this query yet.
    if files.is_some_and(|files| files.query.trim().eq_ignore_ascii_case(needle)) {
        FileState::Ready
    } else {
        FileState::Searching
    }
}

/// The directory a query names outright, if it names one: `/etc`, `~/src`, `~`.
///
/// Pure — it decides what the text *says*, never whether that directory exists. A window opened
/// on a path that is not there says so in the window, which is a better answer than a row that
/// silently fails to appear while you are still typing the path.
pub fn literal_directory(query: &str, home: Option<&Path>) -> Option<PathBuf> {
    let text = query.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('~') {
        let home = home?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return Some(if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        });
    }
    if text.starts_with('/') && text.len() > 1 {
        return Some(PathBuf::from(text));
    }
    None
}

/// The ranked rows for `query` in `scope`, at most `limit` of them.
///
/// An empty query is not a list: recents live under the launcher field, and typing is what
/// asks. On bare Home the answer is applications and only applications; inside the Files
/// object it is the folders and files of that location.
pub fn federate(sources: Sources<'_>, query: &str, scope: &Scope, limit: usize) -> Vec<Row> {
    let needle = query.trim().to_lowercase();
    let mut rows: Vec<Row> = Vec::new();

    if needle.is_empty() {
        return rows;
    }

    match scope {
        Scope::Applications => rows.extend(apps(sources.apps, &needle)),
        Scope::Location(_) => {
            let (directories, files) = files_split(sources.files, &needle);
            rows.extend(capped(directories, DIRECTORY_CAP));
            rows.extend(capped(files, FILE_CAP));
        }
    }

    rows.sort_by(|a, b| {
        (
            a.kind,
            a.hit.source_order(),
            a.label.chars().count(),
            &a.label,
        )
            .cmp(&(
                b.kind,
                b.hit.source_order(),
                b.label.chars().count(),
                &b.label,
            ))
    });
    rows.truncate(limit);
    // A typed path is not a search result: it is an instruction, and it goes first, above
    // whatever the search happens to have found. This is the quick route to a folder window on
    // Home — type where you want to be and it opens there.
    if let Some(path) = literal_directory(query, sources.home) {
        let already = rows
            .iter()
            .any(|row| matches!(&row.hit, Hit::Directory { path: other } if *other == path));
        if !already {
            let label = path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            rows.insert(
                0,
                Row {
                    hit: Hit::Directory { path: path.clone() },
                    label,
                    detail: path.to_string_lossy().into_owned(),
                    kind: MatchKind::ExactName,
                    extra: RowExtra::None,
                },
            );
            rows.truncate(limit);
        }
    }
    // The way out of this search and into the one that covers the whole machine. Appended after
    // the cap, never sorted with the results and never cut by the limit: it is not a result, and
    // a person looking for the row that widens the search must find it in the same place every
    // time — the bottom.
    if matches!(scope, Scope::Location(_))
        && let Ok(id) = SEARCH_EVERYWHERE.parse::<AppId>()
    {
        rows.push(Row {
            hit: Hit::App { id },
            label: SEARCH_EVERYWHERE_LABEL.to_owned(),
            detail: format!("every indexed file, for “{}”", query.trim()),
            kind: MatchKind::NameSubstring,
            extra: RowExtra::Everywhere,
        });
    }
    rows
}

fn capped(mut rows: Vec<Row>, cap: usize) -> Vec<Row> {
    rows.sort_by(|a, b| {
        (a.kind, a.label.chars().count(), &a.label).cmp(&(
            b.kind,
            b.label.chars().count(),
            &b.label,
        ))
    });
    rows.truncate(cap);
    rows
}

fn apps(entries: &[AppEntry], needle: &str) -> Vec<Row> {
    crate::app_search::search(entries, needle)
        .into_iter()
        .map(|row| Row {
            hit: Hit::App {
                id: row.entry.id.clone(),
            },
            label: row.entry.label().to_owned(),
            detail: row.entry.id.as_str().to_owned(),
            kind: row.kind,
            extra: RowExtra::None,
        })
        .collect()
}

/// The file results, split into the two sources they actually are: a directory is somewhere to
/// go and a file is something to open, and mixing them means a search for a folder buries it
/// under the files inside it.
fn files_split(files: Option<FileResults<'_>>, needle: &str) -> (Vec<Row>, Vec<Row>) {
    let Some(files) = files else {
        return (Vec::new(), Vec::new());
    };
    if !files.query.trim().eq_ignore_ascii_case(needle) {
        // Stale: the answer to a question nobody is asking any more.
        return (Vec::new(), Vec::new());
    }
    let mut directories = Vec::new();
    let mut plain = Vec::new();
    for entry in files.entries {
        let Some(kind) = classify(&entry.name, needle) else {
            continue;
        };
        let row = Row {
            hit: if entry.is_dir() {
                Hit::Directory {
                    path: entry.path.clone(),
                }
            } else {
                Hit::File {
                    path: entry.path.clone(),
                }
            },
            label: entry.name.clone(),
            detail: entry
                .path
                .parent()
                .map_or_else(String::new, |parent| parent.display().to_string()),
            kind,
            extra: RowExtra::File {
                size_bytes: entry.size_bytes,
                mtime_ms: entry.mtime_ms,
            },
        };
        if entry.is_dir() {
            directories.push(row);
        } else {
            plain.push(row);
        }
    }
    (directories, plain)
}

/// The same match strength `app_search` measures, over any label at all. One measure across
/// every source is what lets the merge sort by strength before it sorts by source.
fn classify(label: &str, needle: &str) -> Option<MatchKind> {
    let label = label.to_lowercase();
    if label == needle {
        return Some(MatchKind::ExactName);
    }
    if label.starts_with(needle) {
        return Some(MatchKind::NamePrefix);
    }
    if label
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | '('))
        .skip(1)
        .any(|word| word.starts_with(needle))
    {
        return Some(MatchKind::NameWordPrefix);
    }
    label.contains(needle).then_some(MatchKind::NameSubstring)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_typed_path_is_read_as_a_path_and_anything_else_is_not() {
        use std::path::{Path, PathBuf};
        let home = Path::new("/home/dk");
        assert_eq!(
            super::literal_directory("/etc", None),
            Some(PathBuf::from("/etc"))
        );
        assert_eq!(
            super::literal_directory("  ~/src/legeos ", Some(home)),
            Some(PathBuf::from("/home/dk/src/legeos"))
        );
        assert_eq!(super::literal_directory("~", Some(home)), Some(home.into()));
        // A name is a search, not a path; `~` with no home to expand against is neither.
        assert_eq!(super::literal_directory("firefox", Some(home)), None);
        assert_eq!(super::literal_directory("~/src", None), None);
        assert_eq!(super::literal_directory("/", Some(home)), None);
    }

    use super::*;
    use crate::file_browser::EntryKind;

    fn app(id: &str, name: &str) -> AppEntry {
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

    fn entry(path: &str, dir: bool) -> BrowseEntry {
        let path = PathBuf::from(path);
        BrowseEntry {
            name: path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            path,
            kind: if dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            is_symlink: false,
            hidden: false,
            size_bytes: 0,
            mtime_ms: 0,
        }
    }

    /// The row that widens the search is always there, always last, and is never sorted in
    /// among the results or cut by the cap: it is the answer to "not this", and it has to be in
    /// the same place every time.
    #[test]
    fn a_file_search_always_ends_with_the_way_out_of_it() {
        let entries: Vec<BrowseEntry> = (0..30)
            .map(|n| entry(&format!("/home/a/paper-{n}.pdf"), false))
            .collect();
        let apps: Vec<AppEntry> = Vec::new();
        let files = FileResults {
            query: "paper",
            entries: &entries,
        };
        let rows = federate(
            sources(&apps, Some(files)),
            "paper",
            &Scope::Location(FilesLocation::Directory("/home/a".into())),
            TOTAL_CAP,
        );
        let last = rows.last().expect("a search always has this row");
        assert_eq!(last.extra, RowExtra::Everywhere);
        assert_eq!(
            last.hit,
            Hit::App {
                id: SEARCH_EVERYWHERE.parse().unwrap()
            }
        );
        assert!(rows.len() > 1, "and the results above it");
        assert!(
            rows[..rows.len() - 1]
                .iter()
                .all(|row| matches!(row.extra, RowExtra::File { .. })),
            "every result carries the size and time the panel shows"
        );
        assert!(is_panel(&rows), "which makes this list a panel");
    }

    /// A search that found nothing still offers the way out — that is the moment it is most
    /// wanted — while the launcher's own list stays a dropdown with no such row.
    #[test]
    fn the_launcher_has_no_search_everywhere_row_and_is_never_a_panel() {
        let apps = [app("org.legeos.Files", "Files")];
        let launcher = federate(sources(&apps, None), "fil", &Scope::Applications, TOTAL_CAP);
        assert!(!launcher.is_empty());
        assert!(launcher.iter().all(|row| row.extra == RowExtra::None));
        assert!(!is_panel(&launcher));

        let empty = federate(
            sources(&apps, None),
            "nothing-matches-this",
            &Scope::Location(FilesLocation::Directory("/home/a".into())),
            TOTAL_CAP,
        );
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].extra, RowExtra::Everywhere);
        assert!(is_panel(&empty));
    }

    fn labels(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_str()).collect()
    }

    /// The results only. A location's list always ends with the row that hands the question to
    /// the search application, and that row is not an answer to the question.
    fn results(rows: &[Row]) -> Vec<&str> {
        rows.iter()
            .filter(|row| row.extra != RowExtra::Everywhere)
            .map(|row| row.label.as_str())
            .collect()
    }

    fn sources<'a>(apps: &'a [AppEntry], files: Option<FileResults<'a>>) -> Sources<'a> {
        Sources {
            apps,
            files,
            home: None,
        }
    }

    fn in_work() -> Scope {
        Scope::Location(FilesLocation::Directory("/home/a/work".into()))
    }

    /// The bar on bare Home answers with applications and nothing else: a browser's name
    /// finds the browser, and a file that happens to carry the same word is not offered.
    #[test]
    fn home_offers_applications_only() {
        let apps = vec![
            app("org.mozilla.Firefox", "Firefox"),
            app("org.legeos.Notes", "Sticky Notes"),
        ];
        let entries = vec![entry("/home/a/firefox-bookmarks.html", false)];
        let files = Some(FileResults {
            query: "firefox",
            entries: &entries,
        });
        let rows = federate(sources(&apps, files), "firefox", &Scope::Applications, 12);
        assert_eq!(labels(&rows), ["Firefox"]);
        assert!(matches!(rows[0].hit, Hit::App { .. }));
        assert_eq!(rows[0].kind, MatchKind::ExactName);
    }

    /// Inside the Files object the same query is a file search, and applications stay out of
    /// it: the two controls never answer each other's question.
    #[test]
    fn a_location_offers_its_files_and_never_an_application() {
        let apps = vec![app("org.mozilla.Firefox", "Firefox")];
        let entries = vec![entry("/home/a/work/firefox-bookmarks.html", false)];
        let files = Some(FileResults {
            query: "firefox",
            entries: &entries,
        });
        let rows = federate(sources(&apps, files), "firefox", &in_work(), 12);
        assert_eq!(results(&rows), ["firefox-bookmarks.html"]);
    }

    /// Match strength ranks applications: the exact name first, then a word of the name, then
    /// a substring, however they were listed.
    #[test]
    fn strength_ranks_the_applications() {
        let apps = vec![
            app("org.legeos.Notebook", "Notebook"),
            app("org.legeos.Sticky", "Sticky Notes"),
            app("org.legeos.Notes", "Notes"),
        ];
        let rows = federate(sources(&apps, None), "notes", &Scope::Applications, 12);
        assert_eq!(labels(&rows), ["Notes", "Sticky Notes"]);
        assert_eq!(rows[0].kind, MatchKind::ExactName);
        assert_eq!(rows[1].kind, MatchKind::NameWordPrefix);
    }

    /// A file answer for another question is not shown, and its absence reads as "still
    /// searching" rather than as "nothing found" — the difference between a slow answer and a
    /// wrong one.
    #[test]
    fn a_stale_file_answer_is_withheld_and_says_it_is_still_searching() {
        let entries = vec![entry("/home/a/report.pdf", false)];
        let stale = Some(FileResults {
            query: "report",
            entries: &entries,
        });
        assert_eq!(file_state(stale, "repo"), FileState::Searching);
        assert_eq!(file_state(stale, "report"), FileState::Ready);
        assert_eq!(file_state(None, "report"), FileState::Searching);
        assert_eq!(file_state(None, "  "), FileState::Idle);

        let rows = federate(sources(&[], stale), "repo", &in_work(), 12);
        assert!(results(&rows).is_empty(), "{:?}", labels(&rows));
    }

    /// A directory outranks the files inside it at the same strength: a search for a folder must
    /// not be buried under its own contents.
    #[test]
    fn a_directory_precedes_a_file_that_matches_as_well() {
        let entries = vec![
            entry("/home/a/work/report.pdf", false),
            entry("/home/a/report", true),
        ];
        let files = Some(FileResults {
            query: "report",
            entries: &entries,
        });
        let rows = federate(sources(&[], files), "report", &in_work(), 12);
        assert_eq!(results(&rows), ["report", "report.pdf"]);
        assert!(matches!(rows[0].hit, Hit::Directory { .. }));
    }

    /// Many matching files cannot push the matching folders out: each file source is capped
    /// before the merge.
    #[test]
    fn files_are_capped_before_the_merge() {
        let mut entries: Vec<BrowseEntry> = (0..40)
            .map(|n| entry(&format!("/home/a/work/sample-{n}.txt"), false))
            .collect();
        entries.push(entry("/home/a/work/sample", true));
        let files = Some(FileResults {
            query: "sample",
            entries: &entries,
        });
        let rows = federate(sources(&[], files), "sample", &in_work(), TOTAL_CAP);
        assert_eq!(rows[0].label, "sample", "the folder lost its place");
        assert_eq!(results(&rows).len(), 1 + FILE_CAP, "{:?}", labels(&rows));
    }

    /// An empty query is not a list, on Home or inside a location: recents sit under the
    /// launcher field, and typing is what asks.
    #[test]
    fn an_empty_query_is_not_a_dropdown() {
        let apps = vec![
            app("org.legeos.Terminal", "Terminal"),
            app("org.legeos.Files", "Files"),
        ];
        for scope in [Scope::Applications, in_work()] {
            let rows = federate(sources(&apps, None), "  ", &scope, 12);
            assert!(rows.is_empty(), "{rows:?}");
        }
    }

    /// The whole list is a dropdown, not a page.
    #[test]
    fn the_list_is_bounded_however_much_matched() {
        let apps: Vec<AppEntry> = (0..40)
            .map(|n| app(&format!("org.legeos.App{n}"), &format!("Sample {n}")))
            .collect();
        let rows = federate(
            sources(&apps, None),
            "sample",
            &Scope::Applications,
            TOTAL_CAP,
        );
        assert_eq!(rows.len(), TOTAL_CAP);
    }
}
