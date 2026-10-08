//! The file-browser model behind the FileChooser dialog: every decision about *what a person is
//! looking at* — which entries exist, which of them the request's filters admit, whether hidden
//! files are shown, what a click selects and what "Choose" finally answers — with no toolkit,
//! no window and no display anywhere in it.
//!
//! Separated from [`crate::dialog`] on purpose. A browser uploading a file is the most-used
//! portal path there is, and until this module existed none of it could be tested: the whole
//! model lived inside a Pixelkit render loop, and the only way to find out that a directory
//! listing stopped after ten entries was to open the dialog and look at it.
//!
//! The listing itself comes from `templar_filed::list_directory` rather than a private `read_dir`
//! here: `templar-filed` owns directory ordering for every client (`services/templar-filed/src/browse.rs`
//! — directories first, natural filename order), so the portal's chooser and the file manager
//! show a folder the same way round instead of disagreeing about it.

use crate::file_chooser::ChooserKind;
use std::path::{Path, PathBuf};
use templar_filed::{BrowseEntry, browse::MAX_ENTRIES};

/// One pattern from a caller's filter. `Glob` is a shell pattern over the file name; `Mime` is a
/// content type.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FilterPattern {
    Glob(String),
    Mime(String),
}

/// One named filter from the request's `filters` option, e.g. "Images" over `*.png`, `*.jpg`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileFilter {
    pub name: String,
    pub patterns: Vec<FilterPattern>,
}

impl FileFilter {
    /// Whether `file_name` passes this filter.
    ///
    /// A MIME pattern makes the filter admit everything: this process has no content detector,
    /// and inventing one from the extension would hide exactly the file the person came to
    /// upload. Being too permissive costs a person one extra glance; being too strict costs them
    /// the file. Glob matching is case-insensitive, so a camera's `IMG_0001.JPG` still appears
    /// under a `*.jpg` filter.
    pub fn accepts(&self, file_name: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        for pattern in &self.patterns {
            match pattern {
                FilterPattern::Mime(_) => return true,
                FilterPattern::Glob(glob) => {
                    if glob_matches(glob, file_name) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

type GlobChars<'a> = std::iter::Peekable<std::iter::FlatMap<
    std::str::Chars<'a>, std::char::ToLowercase, fn(char) -> std::char::ToLowercase,
>>;

fn glob_chars(value: &str) -> GlobChars<'_> {
    value.chars().flat_map(char::to_lowercase as fn(char) -> std::char::ToLowercase).peekable()
}

/// Match one folded glob atom, including the bracket classes browsers use for
/// extension filters. Unclosed classes are literal; no regex, recursion or heap scratch.
fn glob_atom(pattern: &mut GlobChars<'_>, value: char) -> bool {
    match pattern.next() {
        Some('?') => true,
        Some('\\') => pattern.next() == Some(value),
        Some('[') => {
            let mut class = pattern.clone();
            let negated = matches!(class.peek(), Some('!' | '^'));
            if negated { class.next(); }
            let mut first = true;
            let mut matched = false;
            while let Some(mut character) = class.next() {
                if character == ']' && !first {
                    *pattern = class;
                    return matched != negated;
                }
                if character == '\\' {
                    let Some(escaped) = class.next() else { break };
                    character = escaped;
                }
                let mut range = class.clone();
                if range.next() == Some('-')
                    && let Some(mut end) = range.next()
                    && end != ']'
                {
                    if end == '\\' {
                        let Some(escaped) = range.next() else { break };
                        end = escaped;
                    }
                    matched |= character <= value && value <= end;
                    class = range;
                } else {
                    matched |= character == value;
                }
                first = false;
            }
            value == '['
        }
        Some(character) => character == value,
        None => false,
    }
}

/// Case-insensitive shell globs, iteratively backtracked without allocating
/// lowercased copies for every visible filename on every frame.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let mut pattern = glob_chars(pattern);
    let mut name = glob_chars(name);
    let mut star = None;
    while let Some(value) = name.peek().copied() {
        if pattern.peek() == Some(&'*') {
            pattern.next();
            star = Some((pattern.clone(), name.clone()));
            continue;
        }
        let mut next = pattern.clone();
        if glob_atom(&mut next, value) {
            pattern = next;
            name.next();
            continue;
        }
        let Some((after_star, remainder)) = star.as_mut() else { return false };
        if remainder.next().is_none() { return false; }
        pattern = after_star.clone();
        name = remainder.clone();
    }
    pattern.all(|character| character == '*')
}

/// What one activation of a row means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activation {
    /// The browser moved into a directory; the caller must re-read the listing.
    Entered(PathBuf),
    /// A file's selection state changed.
    Picked(PathBuf),
}

/// The chooser's whole state, minus the pixels.
#[derive(Clone, Debug)]
pub struct Browser {
    kind: ChooserKind,
    directory: PathBuf,
    entries: Vec<BrowseEntry>,
    truncated: bool,
    show_hidden: bool,
    filters: Vec<FileFilter>,
    current_filter: Option<usize>,
    selected: Vec<PathBuf>,
}

impl Browser {
    /// A browser positioned at the request's starting directory, with its filters. Does no I/O:
    /// call [`Browser::refresh`] (or [`Browser::set_listing`], in tests) to fill the listing.
    pub fn new(
        kind: ChooserKind,
        directory: PathBuf,
        filters: Vec<FileFilter>,
        current_filter: Option<usize>,
    ) -> Self {
        Self {
            kind,
            directory,
            entries: Vec::new(),
            truncated: false,
            show_hidden: false,
            filters,
            current_filter,
            selected: Vec::new(),
        }
    }

    pub fn kind(&self) -> &ChooserKind {
        &self.kind
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Whether the listing hit `templar-filed`'s bound and is therefore incomplete — shown to the
    /// person rather than swallowed, so a folder of 900 files never silently looks like 500.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn filters(&self) -> &[FileFilter] {
        &self.filters
    }

    pub fn current_filter(&self) -> Option<usize> {
        self.current_filter
    }

    /// Select filter `index` (`None` = every file). Out-of-range indices are ignored rather than
    /// panicking: the index arrives from a caller's `current_filter` option.
    pub fn set_current_filter(&mut self, index: Option<usize>) {
        match index {
            Some(index) if index < self.filters.len() => self.current_filter = Some(index),
            Some(_) => {}
            None => self.current_filter = None,
        }
        if !matches!(self.kind, ChooserKind::Open { directory: true, .. }) {
            let filter = self.current_filter.and_then(|index| self.filters.get(index));
            self.selected.retain(|path| path.file_name().is_some_and(|name|
                filter.is_none_or(|filter| filter.accepts(&name.to_string_lossy()))));
        }
    }

    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected.iter().any(|selected| selected == path)
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Replace the listing wholesale. The entries are used in the order given — `templar-filed`
    /// already sorted them.
    pub fn set_listing(&mut self, entries: Vec<BrowseEntry>, truncated: bool) {
        self.entries = entries;
        self.truncated = truncated;
    }

    /// Re-read the current directory through `templar-filed`. An unreadable directory yields an
    /// empty listing rather than an error dialog: the person can still navigate out of it.
    pub fn refresh(&mut self) {
        let listing = templar_filed::list_directory(&self.directory, self.show_hidden, MAX_ENTRIES);
        match listing {
            Ok(listing) => self.set_listing(listing.entries, listing.truncated),
            Err(error) => {
                tracing::debug!(%error, directory = %self.directory.display(), "listing a directory");
                self.set_listing(Vec::new(), false);
            }
        }
    }

    /// Show or hide dot-files. The caller refreshes afterwards — `templar-filed` filters hidden
    /// entries at read time, so the flag changes what the next listing contains.
    pub fn set_show_hidden(&mut self, show_hidden: bool) {
        self.show_hidden = show_hidden;
    }

    /// The entries a person should see: every listed entry the current filter admits.
    /// Directories always survive filtering — a filter narrows which files may be *chosen*, never
    /// which folders may be *entered*.
    pub fn visible(&self) -> Vec<&BrowseEntry> {
        let filter = self
            .current_filter
            .and_then(|index| self.filters.get(index));
        self.entries
            .iter()
            .filter(|entry| {
                (entry.is_dir() || !matches!(self.kind, ChooserKind::Open { directory: true, .. }))
                    && (entry.is_dir() || filter.is_none_or(|filter| filter.accepts(&entry.name)))
            })
            .collect()
    }

    /// Move to `directory`, dropping a selection that no longer makes sense there.
    pub fn enter(&mut self, directory: PathBuf) {
        self.directory = directory;
        if !matches!(self.kind, ChooserKind::Open { multiple: true, directory: true }) {
            self.selected.clear();
        }
    }

    /// Move to the parent directory, if there is one.
    pub fn go_up(&mut self) -> bool {
        let Some(parent) = self.directory.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.enter(parent);
        true
    }

    /// Activate one visible entry: a directory is entered, a file is picked.
    pub fn activate(&mut self, index: usize) -> Option<Activation> {
        let entry = self.visible().get(index).map(|entry| (*entry).clone())?;
        if entry.is_dir() {
            self.enter(entry.path.clone());
            return Some(Activation::Entered(entry.path));
        }
        self.pick(entry.path.clone());
        Some(Activation::Picked(entry.path))
    }

    /// Apply a click on a file: a multi-select toggles, anything else replaces.
    pub fn pick(&mut self, path: PathBuf) {
        if matches!(self.kind, ChooserKind::Open { multiple: true, .. }) {
            if let Some(at) = self.selected.iter().position(|selected| *selected == path) {
                self.selected.remove(at);
            } else {
                self.selected.push(path);
            }
        } else {
            self.selected.clear();
            self.selected.push(path);
        }
    }

    /// What "Choose" answers, or the sentence to show the person instead.
    ///
    /// `save_name` is the dialog's name field; a relative name is resolved against the directory
    /// on screen, so "choose the folder, then type the name" works the way a save dialog should.
    pub fn choose(&self, save_name: &str) -> Result<Vec<PathBuf>, &'static str> {
        match &self.kind {
            ChooserKind::Open {
                multiple, directory: true,
            } => Ok(if *multiple && !self.selected.is_empty() {
                self.selected.clone()
            } else {
                vec![self.directory.clone()]
            }),
            ChooserKind::Open { multiple, .. } => {
                if self.selected.is_empty() {
                    return Err("Choose at least one file.");
                }
                if *multiple {
                    Ok(self.selected.clone())
                } else {
                    Ok(vec![self.selected[0].clone()])
                }
            }
            // `SaveFiles` asks for a *directory*: the application already named the files, and
            // the answer is those names inside whatever folder the person navigated to.
            ChooserKind::SaveFiles { file_names } if !file_names.is_empty() => {
                Ok(self.save_files_targets())
            }
            ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. } => {
                let candidate = PathBuf::from(save_name);
                if save_name.trim().is_empty() {
                    return Err("Enter a file name.");
                }
                let target = if candidate.is_absolute() {
                    candidate
                } else {
                    self.directory.join(candidate)
                };
                if target.is_dir() {
                    return Err("Choose a file name, not a folder.");
                }
                Ok(vec![target])
            }
        }
    }

    /// The names a `SaveFiles` request asked to write into the chosen directory, joined onto it.
    pub fn save_files_targets(&self) -> Vec<PathBuf> {
        match &self.kind {
            ChooserKind::SaveFiles { file_names } => file_names
                .iter()
                .map(|name| self.directory.join(name))
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_filed::EntryKind;

    fn entry(name: &str, kind: EntryKind) -> BrowseEntry {
        BrowseEntry {
            path: PathBuf::from("/home/u").join(name),
            name: name.to_owned(),
            kind,
            is_symlink: false,
            hidden: name.starts_with('.'),
            size_bytes: 0,
            mtime_ms: 0,
        }
    }

    fn file(name: &str) -> BrowseEntry {
        entry(name, EntryKind::File)
    }

    fn open_browser(multiple: bool) -> Browser {
        Browser::new(
            ChooserKind::Open {
                multiple,
                directory: false,
            },
            PathBuf::from("/home/u"),
            Vec::new(),
            None,
        )
    }

    #[test]
    fn browser_extension_classes_select_files_without_hiding_the_requested_type() {
        let text = FileFilter {
            name: "*.txt".into(),
            patterns: vec![FilterPattern::Glob("*.[tT][xX][tT]".into())],
        };
        assert!(text.accepts("fixture.txt"));
        assert!(text.accepts("SECOND.TXT"));
        assert!(!text.accepts("picture.png"));
        assert!(!glob_matches("report-[0-9]?.[!p]df", "report-42.pdf"));
        assert!(glob_matches("report-[0-9]?.[!p]df", "report-42.adf"));
        assert!(glob_matches("literal\\*.txt", "literal*.txt"));
        assert!(glob_matches("unclosed[.txt", "unclosed[.txt"));
        assert!(glob_matches("*.txt", "名前😀.txt"));
    }

    /// The model this module exists for. The bug it starts from: the dialog truncated every
    /// listing to ten entries, so a browser upload from a real folder could not reach the
    /// eleventh file. The rest is what a person does next — navigate, select, name a save.
    #[test]
    fn dialog_model_lists_all_entries() {
        let mut browser = open_browser(false);
        let names: Vec<String> = (0..37).map(|index| format!("file{index:02}.txt")).collect();
        browser.set_listing(names.iter().map(|name| file(name)).collect(), false);

        let visible = browser.visible();
        assert_eq!(visible.len(), 37, "every listed entry must be reachable");
        assert_eq!(visible[36].name, "file36.txt");
        assert!(!browser.truncated(), "and a complete listing says so");

        // Hidden entries are `templar-filed`'s to exclude; the flag is what asks it to.
        assert!(!browser.show_hidden());
        browser.set_show_hidden(true);
        browser.set_listing(
            vec![
                entry("Pictures", EntryKind::Directory),
                file(".bashrc"),
                file("a.txt"),
            ],
            true,
        );
        assert_eq!(browser.visible().len(), 3);
        assert!(
            browser.truncated(),
            "a bounded listing is admitted, not hidden"
        );

        // Choosing needs a selection; a click on a file is one, and a click on a directory
        // navigates instead and drops a selection that no longer applies.
        assert_eq!(browser.choose(""), Err("Choose at least one file."));
        assert_eq!(
            browser.activate(2),
            Some(Activation::Picked(PathBuf::from("/home/u/a.txt")))
        );
        assert!(browser.is_selected(Path::new("/home/u/a.txt")));
        assert_eq!(browser.choose(""), Ok(vec![PathBuf::from("/home/u/a.txt")]));
        assert_eq!(
            browser.activate(0),
            Some(Activation::Entered(PathBuf::from("/home/u/Pictures")))
        );
        assert_eq!(browser.directory(), Path::new("/home/u/Pictures"));
        assert_eq!(browser.choose(""), Err("Choose at least one file."));
        assert!(browser.go_up());
        assert_eq!(browser.directory(), Path::new("/home/u"));

        // A multi-select toggles where a single-select replaces.
        let mut multi = open_browser(true);
        multi.pick(PathBuf::from("/home/u/a.txt"));
        multi.pick(PathBuf::from("/home/u/b.txt"));
        assert_eq!(multi.choose("").map(|paths| paths.len()), Ok(2));
        multi.pick(PathBuf::from("/home/u/a.txt"));
        assert_eq!(multi.choose(""), Ok(vec![PathBuf::from("/home/u/b.txt")]));

        // Saving answers the name field, resolved against the folder on screen.
        let saving = Browser::new(
            ChooserKind::Save { current_name: None },
            PathBuf::from("/home/u/Downloads"),
            Vec::new(),
            None,
        );
        assert_eq!(saving.choose("  "), Err("Enter a file name."));
        assert_eq!(
            saving.choose("report.pdf"),
            Ok(vec![PathBuf::from("/home/u/Downloads/report.pdf")])
        );
        assert_eq!(
            saving.choose("/elsewhere/report.pdf"),
            Ok(vec![PathBuf::from("/elsewhere/report.pdf")])
        );

        // `SaveFiles` names its own files and asks only for the folder to put them in.
        let save_files = Browser::new(
            ChooserKind::SaveFiles {
                file_names: vec!["one.txt".into(), "two.txt".into()],
            },
            PathBuf::from("/home/u/Downloads"),
            Vec::new(),
            None,
        );
        assert_eq!(
            save_files.choose(""),
            Ok(vec![
                PathBuf::from("/home/u/Downloads/one.txt"),
                PathBuf::from("/home/u/Downloads/two.txt"),
            ])
        );
    }

    #[test]
    fn changing_filter_drops_files_no_longer_visible_in_multi_selection() {
        let mut browser = Browser::new(
            ChooserKind::Open { multiple: true, directory: false },
            PathBuf::from("/home/u"),
            vec![
                FileFilter { name: "Text".into(), patterns: vec![FilterPattern::Glob("*.txt".into())] },
                FileFilter { name: "Images".into(), patterns: vec![FilterPattern::Glob("*.png".into())] },
            ], None);
        browser.set_listing(vec![file("a.txt"), file("b.png")], false);
        browser.activate(0);
        browser.activate(1);
        browser.set_current_filter(Some(1));
        assert_eq!(browser.choose(""), Ok(vec![PathBuf::from("/home/u/b.png")]));
        assert!(!browser.is_selected(Path::new("/home/u/a.txt")));
    }

    #[test]
    fn directory_multi_selection_is_not_replaced_by_navigation() {
        let mut browser = Browser::new(
            ChooserKind::Open { multiple: true, directory: true },
            PathBuf::from("/home/u"), Vec::new(), None);
        browser.set_listing(vec![
            entry("one", EntryKind::Directory),
            file("not-a-directory.txt"),
            entry("two", EntryKind::Directory),
        ], false);
        assert_eq!(browser.visible().len(), 2);
        assert_eq!(browser.choose(""), Ok(vec![PathBuf::from("/home/u")]));
        browser.pick(PathBuf::from("/home/u/one"));
        browser.pick(PathBuf::from("/home/u/two"));
        browser.activate(0);
        assert_eq!(browser.directory(), Path::new("/home/u/one"));
        assert_eq!(browser.choose(""), Ok(vec![
            PathBuf::from("/home/u/one"), PathBuf::from("/home/u/two"),
        ]));
        browser.pick(PathBuf::from("/home/u/one"));
        assert_eq!(browser.choose(""), Ok(vec![PathBuf::from("/home/u/two")]));
    }

    #[test]
    fn single_directory_choose_returns_the_navigated_folder() {
        let mut browser = Browser::new(
            ChooserKind::Open { multiple: false, directory: true },
            PathBuf::from("/home/u"), Vec::new(), None);
        browser.set_listing(vec![entry("one", EntryKind::Directory)], false);
        browser.activate(0);
        assert_eq!(browser.choose(""), Ok(vec![PathBuf::from("/home/u/one")]));
    }

    #[test]
    fn save_preserves_nonempty_filename_whitespace() {
        let browser = Browser::new(ChooserKind::Save { current_name: None },
            PathBuf::from("/home/u"), Vec::new(), None);
        assert_eq!(browser.choose(" report.txt "),
            Ok(vec![PathBuf::from("/home/u/ report.txt ")]));
    }
}
