//! One search control for the whole desktop.
//!
//! The shell used to have three. The bar at the top of Home searched applications; a second
//! field appeared inside an expanded drawer and searched files; and the launcher tile filtered a
//! list from the first one's buffer. Three controls, three vocabularies, and no way to ask "where
//! is that thing" without first deciding what kind of thing it was.
//!
//! This federates. One query goes to every source the shell already has — the applications
//! `AppIdentity1` listed, the projects `Projects1` listed, the last file search `Files.Search1`
//! completed, and the declared action manifest — and comes back as one ranked list.
//!
//! Three rules make the result trustworthy rather than merely full:
//!
//! 1. **Match strength first, source second.** `app_search::MatchKind` is the universal measure:
//!    an exact name in any source beats a substring in any other. Only when two rows match
//!    equally well does the source break the tie.
//! 2. **Every source is capped before the merge**, so one source with a thousand hits cannot
//!    crowd out the four rows another had.
//! 3. **A late answer never reorders what is on screen.** File search is asynchronous; a result
//!    whose query is not the one being asked is not shown at all, and its absence says "still
//!    searching" rather than "nothing found".
//!
//! Pure: sources in, rows out. No D-Bus, no `Ui`, no clock.

use std::path::PathBuf;

use lege_types::AppId;

use crate::app_search::{AppEntry, MatchKind};
use crate::field_model::ProjectInfo;
use crate::file_browser::BrowseEntry;
use crate::files_surface::FilesLocation;

/// What one row stands for, and everything acting on it needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hit {
    Project {
        id: String,
    },
    App {
        id: AppId,
    },
    Directory {
        path: PathBuf,
    },
    File {
        path: PathBuf,
    },
    Action {
        id: &'static str,
    },
    /// The permanent escape from a scoped search: ask the whole desktop instead of this one
    /// location. Never ranked — it is always the last row, so it does not move as you type.
    Everything,
}

impl Hit {
    /// Which source a hit came from, as the tiebreak between two equally good matches.
    ///
    /// Projects first because a project is the largest thing a person can be looking for and the
    /// one they name most deliberately; actions last because an action is what you fall back to
    /// when nothing you own matched.
    fn source_order(&self) -> u8 {
        match self {
            Self::Project { .. } => 0,
            Self::App { .. } => 1,
            Self::Directory { .. } => 2,
            Self::File { .. } => 3,
            Self::Action { .. } => 4,
            Self::Everything => 5,
        }
    }
}

/// One row of the dropdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub hit: Hit,
    pub label: String,
    /// The second line: a path, a project's state, what an action does.
    pub detail: String,
    pub kind: MatchKind,
}

/// Where the question is being asked from. Derived from the surface, never chosen by the user —
/// there is no mode to be in and no mode to get stuck in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Bare Home: everything the shell knows about.
    Everything,
    /// Inside the expanded Files object: this location, with a permanent way out.
    Location(FilesLocation),
}

/// The last file search that completed, and the query it answered.
#[derive(Clone, Copy, Debug)]
pub struct FileResults<'a> {
    pub query: &'a str,
    pub entries: &'a [BrowseEntry],
}

/// Everything the omnibox can see. All of it is already in the shell's hands: federating is
/// composition, not a new fetch, and deliberately not a server-side search interface.
#[derive(Clone, Copy, Debug)]
pub struct Sources<'a> {
    pub apps: &'a [AppEntry],
    pub projects: &'a [ProjectInfo],
    /// `None` while nothing has come back yet, which is a different thing from a search that
    /// found nothing — see [`file_state`].
    pub files: Option<FileResults<'a>>,
    /// The declared actions, offerable by name (`lege_types::action::MANIFEST`).
    pub actions: &'static [lege_types::action::ActionDecl],
}

/// How many rows each source may contribute before the merge.
///
/// Caps before the merge, not after: capping the merged list would let one prolific source
/// silently push another out entirely, and a search that stops showing your projects the moment
/// a directory has many files is a search you stop trusting.
const APP_CAP: usize = 5;
const PROJECT_CAP: usize = 3;
const DIRECTORY_CAP: usize = 3;
const FILE_CAP: usize = 6;
const ACTION_CAP: usize = 3;
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

/// The ranked rows for `query` in `scope`, at most `limit` of them.
///
/// An empty query is not "match nothing": on bare Home it is every application, which is what a
/// launcher shows before anyone types.
pub fn federate(sources: Sources<'_>, query: &str, scope: &Scope, limit: usize) -> Vec<Row> {
    let needle = query.trim().to_lowercase();
    let mut rows: Vec<Row> = Vec::new();

    if needle.is_empty() {
        // Nothing to rank, so nothing is ranked: the applications in their own stable
        // alphabetical order, which is the list a person scans rather than reads.
        rows.extend(
            crate::app_search::all_offerable(sources.apps)
                .into_iter()
                .take(limit)
                .map(|row| Row {
                    hit: Hit::App {
                        id: row.entry.id.clone(),
                    },
                    label: row.entry.label().to_owned(),
                    detail: row.entry.id.as_str().to_owned(),
                    kind: row.kind,
                }),
        );
        push_escape(&mut rows, scope);
        return rows;
    }

    rows.extend(capped(projects(sources.projects, &needle), PROJECT_CAP));
    rows.extend(capped(apps(sources.apps, &needle), APP_CAP));
    let (directories, files) = files_split(sources.files, &needle);
    rows.extend(capped(directories, DIRECTORY_CAP));
    rows.extend(capped(files, FILE_CAP));
    rows.extend(capped(actions(sources.actions, &needle), ACTION_CAP));

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
    push_escape(&mut rows, scope);
    rows
}

/// The escape row, when there is somewhere to escape from. Appended after the truncation and
/// after the sort, so it is always the last row and never moves as the ranking changes.
fn push_escape(rows: &mut Vec<Row>, scope: &Scope) {
    let Scope::Location(location) = scope else {
        return;
    };
    rows.push(Row {
        hit: Hit::Everything,
        label: "Search everything".to_owned(),
        detail: format!("Currently searching {}", location.label()),
        kind: MatchKind::Identifier,
    });
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
        })
        .collect()
}

fn projects(projects: &[ProjectInfo], needle: &str) -> Vec<Row> {
    projects
        .iter()
        .filter_map(|project| {
            let label = if project.name.is_empty() {
                project.id.clone()
            } else {
                project.name.clone()
            };
            classify(&label, needle)
                .or_else(|| classify(&project.id, needle).map(|_| MatchKind::Identifier))
                .map(|kind| Row {
                    hit: Hit::Project {
                        id: project.id.clone(),
                    },
                    label,
                    // Where it lives, which is what tells two similarly named projects apart —
                    // with its state only when the state is worth saying. An archived project is
                    // still searchable and still openable; the badge is a note, not a lock.
                    detail: match (project.root.as_str(), project.archived) {
                        ("", false) => "Project".to_owned(),
                        ("", true) => "Project · archived".to_owned(),
                        (root, false) => root.to_owned(),
                        (root, true) => format!("{root} · archived"),
                    },
                    kind,
                })
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
        };
        if entry.is_dir() {
            directories.push(row);
        } else {
            plain.push(row);
        }
    }
    (directories, plain)
}

fn actions(actions: &'static [lege_types::action::ActionDecl], needle: &str) -> Vec<Row> {
    actions
        .iter()
        .filter_map(|action| {
            classify(action.label, needle).map(|kind| Row {
                hit: Hit::Action { id: action.id },
                label: action.label.to_owned(),
                // Its accelerator if it has one — the useful thing to learn from seeing an
                // action in a list — and otherwise the id, which at least says where it lives.
                detail: action.primary_chord().map_or_else(
                    || action.id.to_owned(),
                    |chord| chord.to_string().to_uppercase(),
                ),
                kind,
            })
        })
        .collect()
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
        }
    }

    fn project(id: &str, name: &str, open: bool) -> ProjectInfo {
        ProjectInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            is_open: open,
            ..ProjectInfo::default()
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

    fn labels(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_str()).collect()
    }

    const NO_ACTIONS: &[lege_types::action::ActionDecl] = &[];

    fn sources<'a>(
        apps: &'a [AppEntry],
        projects: &'a [ProjectInfo],
        files: Option<FileResults<'a>>,
    ) -> Sources<'a> {
        Sources {
            apps,
            projects,
            files,
            actions: NO_ACTIONS,
        }
    }

    /// Match strength decides first and the source only breaks a tie. "notes" is a project's
    /// whole name, a word of an application's, and part of a file's — and they come back in that
    /// order regardless of which source they are from.
    #[test]
    fn strength_ranks_before_source_and_source_only_breaks_ties() {
        let apps = vec![app("org.legeos.Notes", "Sticky Notes")];
        let projects = vec![project("notes", "Notes", true)];
        let entries = vec![entry("/home/a/my-notes-backup.md", false)];
        let files = Some(FileResults {
            query: "notes",
            entries: &entries,
        });
        let rows = federate(
            sources(&apps, &projects, files),
            "notes",
            &Scope::Everything,
            12,
        );
        assert_eq!(
            labels(&rows),
            ["Notes", "Sticky Notes", "my-notes-backup.md"]
        );
        assert_eq!(rows[0].kind, MatchKind::ExactName);
        assert_eq!(rows[1].kind, MatchKind::NameWordPrefix);
    }

    /// Two rows that match equally well are ordered by source, so a project always precedes the
    /// application of the same name.
    #[test]
    fn an_equal_match_puts_the_project_before_the_application() {
        let apps = vec![app("org.legeos.Thesis", "Thesis")];
        let projects = vec![project("thesis", "Thesis", false)];
        let rows = federate(
            sources(&apps, &projects, None),
            "thesis",
            &Scope::Everything,
            12,
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.hit.source_order())
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    /// A source with many hits is capped before the merge, so it cannot crowd out a source with
    /// few. Ten matching applications leave the one matching project exactly where it was.
    #[test]
    fn every_source_is_capped_before_the_merge() {
        let apps: Vec<AppEntry> = (0..10)
            .map(|n| app(&format!("org.legeos.App{n}"), &format!("Sample {n}")))
            .collect();
        let projects = vec![project("sample", "Sample", true)];
        let rows = federate(
            sources(&apps, &projects, None),
            "sample",
            &Scope::Everything,
            12,
        );
        assert_eq!(rows[0].label, "Sample", "the project lost its place");
        assert_eq!(
            rows.len(),
            1 + APP_CAP,
            "the application source was not capped: {:?}",
            labels(&rows)
        );
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

        let rows = federate(sources(&[], &[], stale), "repo", &Scope::Everything, 12);
        assert!(rows.is_empty(), "{:?}", labels(&rows));
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
        let rows = federate(sources(&[], &[], files), "report", &Scope::Everything, 12);
        assert_eq!(labels(&rows), ["report", "report.pdf"]);
        assert!(matches!(rows[0].hit, Hit::Directory { .. }));
    }

    /// Scoped to a location, the escape row is always present and always last — it does not move
    /// as the ranking changes underneath it, because it is appended after both the sort and the
    /// truncation.
    #[test]
    fn a_scoped_search_always_ends_with_the_way_out() {
        let apps = vec![app("org.legeos.Files", "Files")];
        let scope = Scope::Location(FilesLocation::Directory("/home/a/work".into()));
        for query in ["", "files", "nothing at all"] {
            let rows = federate(sources(&apps, &[], None), query, &scope, 12);
            assert_eq!(
                rows.last().map(|row| &row.hit),
                Some(&Hit::Everything),
                "{query:?}: no way out"
            );
        }
        // And on bare Home there is nothing to escape from, so there is no row for it.
        let rows = federate(sources(&apps, &[], None), "files", &Scope::Everything, 12);
        assert!(rows.iter().all(|row| row.hit != Hit::Everything));
    }

    /// An empty query is "no filter", not "match nothing": it is every application, in the
    /// stable alphabetical order a person scans.
    #[test]
    fn an_empty_query_offers_every_application_in_a_stable_order() {
        let apps = vec![
            app("org.legeos.Terminal", "Terminal"),
            app("org.legeos.Files", "Files"),
        ];
        let rows = federate(sources(&apps, &[], None), "  ", &Scope::Everything, 12);
        assert_eq!(labels(&rows), ["Files", "Terminal"]);
    }

    /// The whole list is a dropdown, not a page.
    #[test]
    fn the_merged_list_is_bounded_however_much_matched() {
        let apps: Vec<AppEntry> = (0..40)
            .map(|n| app(&format!("org.legeos.App{n}"), &format!("Sample {n}")))
            .collect();
        let projects: Vec<ProjectInfo> = (0..40)
            .map(|n| project(&format!("p{n}"), &format!("Sample project {n}"), false))
            .collect();
        let rows = federate(
            sources(&apps, &projects, None),
            "sample",
            &Scope::Everything,
            TOTAL_CAP,
        );
        assert!(rows.len() <= TOTAL_CAP, "{} rows", rows.len());
    }

    /// A declared action is reachable by its own name, and is the last source to be offered.
    #[test]
    fn a_declared_action_is_findable_by_name_and_ranks_last() {
        let apps = vec![app("org.legeos.Session", "Session")];
        let rows = federate(
            Sources {
                apps: &apps,
                projects: &[],
                files: None,
                actions: lege_types::action::MANIFEST,
            },
            "log out",
            &Scope::Everything,
            12,
        );
        assert!(
            rows.iter().any(|row| matches!(row.hit, Hit::Action { .. })),
            "{:?}",
            labels(&rows)
        );
    }
}
