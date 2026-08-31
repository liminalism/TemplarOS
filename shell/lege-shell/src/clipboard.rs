//! What copy and cut put down, and what paste picks up.
//!
//! Two jobs, and they are worth separating. The *internal* clipboard is a path set and a mode,
//! and it is what makes Ctrl+C and Ctrl+V work inside Home. The *interchange* format is
//! `text/uri-list` (`lege_types::uri_list`), and it is what makes the same keystrokes work
//! between Home, the Files app and every GTK or Qt application on the machine.
//!
//! Cut is the awkward half. `text/uri-list` carries paths and nothing else, so a receiver has no
//! way to know a cut was intended — which is why GNOME invented `x-special/gnome-copied-files`
//! ("copy" or "cut", then the URIs) and why everything else came to read it. Both are produced
//! and both are read here: the uri-list so anything at all can paste what Home copied, and the
//! GNOME target so a cut survives the trip. Neither is guessed at; when only a uri-list arrives,
//! the paste is a copy, because copying too much is recoverable and moving something that was
//! only meant to be copied is not.
//!
//! Nothing here talks to a compositor. This is the model and the encoding, both pure and both
//! tested without a clipboard; `crate::surface::app` owns the one instance and the wiring.

use lege_types::uri_list;
use std::path::{Path, PathBuf};

/// The MIME type that carries a cut/copy distinction between applications.
pub const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardMode {
    Copy,
    /// The sources move on paste, and are gone from where they were.
    Cut,
}

impl ClipboardMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Cut => "cut",
        }
    }
}

/// A path set waiting to be pasted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Clipboard {
    paths: Vec<PathBuf>,
    mode: ClipboardMode,
}

/// What a paste asks the file service to do: one operation over the whole set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasteRequest {
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub copy: bool,
}

impl Clipboard {
    pub fn copy(paths: Vec<PathBuf>) -> Option<Clipboard> {
        Self::new(paths, ClipboardMode::Copy)
    }

    pub fn cut(paths: Vec<PathBuf>) -> Option<Clipboard> {
        Self::new(paths, ClipboardMode::Cut)
    }

    /// `None` for an empty selection: an empty clipboard is not a clipboard, and putting one
    /// down would silently discard whatever was on it.
    fn new(paths: Vec<PathBuf>, mode: ClipboardMode) -> Option<Clipboard> {
        (!paths.is_empty()).then_some(Clipboard { paths, mode })
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn mode(&self) -> ClipboardMode {
        self.mode
    }

    /// The `text/uri-list` payload to offer alongside [`Self::to_gnome_copied_files`].
    pub fn to_uri_list(&self) -> String {
        uri_list::encode(&self.paths)
    }

    /// The GNOME target, which is the only widely-read way to say "this was a cut".
    pub fn to_gnome_copied_files(&self) -> String {
        let mut out = String::from(self.mode.as_str());
        for path in &self.paths {
            out.push('\n');
            out.push_str(&uri_list::encode_path(path));
        }
        out
    }

    /// Read what another application put down.
    ///
    /// The GNOME target is preferred because it is the one that carries the mode; a bare
    /// uri-list is read as a copy. Not as a cut: a paste that copies too much leaves a spare file
    /// to delete, and a paste that moves what was only meant to be copied takes the original
    /// away from wherever it was needed.
    pub fn from_foreign(
        gnome_copied_files: Option<&str>,
        uri_list_payload: Option<&str>,
    ) -> Option<Clipboard> {
        if let Some(payload) = gnome_copied_files
            && let Some(clipboard) = Self::from_gnome_copied_files(payload)
        {
            return Some(clipboard);
        }
        Self::copy(uri_list::decode(uri_list_payload?))
    }

    fn from_gnome_copied_files(payload: &str) -> Option<Clipboard> {
        let mut lines = payload.lines();
        let mode = match lines.next()?.trim() {
            "cut" => ClipboardMode::Cut,
            "copy" => ClipboardMode::Copy,
            // An unknown verb is not a licence to guess at moving somebody's files.
            _ => return None,
        };
        let paths: Vec<PathBuf> = lines
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(uri_list::decode_uri)
            .collect();
        Self::new(paths, mode)
    }

    /// The one operation pasting into `destination` performs, or `None` when it would do nothing.
    ///
    /// A source that *is* the destination is dropped rather than sent: the service would refuse
    /// it, and a refusal the shell could have avoided reads as a bug in the paste.
    pub fn paste_into(&self, destination: &Path) -> Option<PasteRequest> {
        let sources: Vec<PathBuf> = self
            .paths
            .iter()
            .filter(|path| path.as_path() != destination)
            .cloned()
            .collect();
        (!sources.is_empty()).then(|| PasteRequest {
            sources,
            destination: destination.to_path_buf(),
            copy: self.mode == ClipboardMode::Copy,
        })
    }

    /// Whether a completed paste consumed this clipboard.
    ///
    /// A cut is spent by the paste that honours it: the sources are no longer where they were, so
    /// offering to paste them again would be offering to move files that have already moved.
    /// A copy stays, because pasting the same thing into three folders is an ordinary thing to do.
    pub fn spent_by_paste(&self) -> bool {
        self.mode == ClipboardMode::Cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/one.txt"),
            PathBuf::from("/home/u/two.txt"),
        ]
    }

    #[test]
    fn clipboard_offers_a_uri_list_anything_can_paste() {
        let clipboard = Clipboard::copy(paths()).unwrap();
        assert_eq!(
            clipboard.to_uri_list(),
            "file:///home/u/one.txt\r\nfile:///home/u/two.txt\r\n"
        );
        // Round trip through the interchange format alone.
        assert_eq!(
            Clipboard::from_foreign(None, Some(&clipboard.to_uri_list())),
            Some(clipboard)
        );
    }

    #[test]
    fn clipboard_carries_a_cut_across_applications_through_the_gnome_target() {
        let cut = Clipboard::cut(paths()).unwrap();
        assert_eq!(
            cut.to_gnome_copied_files(),
            "cut\nfile:///home/u/one.txt\nfile:///home/u/two.txt"
        );
        let read = Clipboard::from_foreign(Some(&cut.to_gnome_copied_files()), None).unwrap();
        assert_eq!(read.mode(), ClipboardMode::Cut);
        assert_eq!(read.paths(), paths());
        // And the uri-list is still offered beside it, so a receiver that reads only that one
        // still gets the files.
        assert_eq!(cut.to_uri_list().lines().count(), 2);
    }

    #[test]
    fn clipboard_reads_a_bare_uri_list_as_a_copy_never_as_a_cut() {
        // Copying too much leaves a spare file to delete; moving what was only meant to be
        // copied takes the original away from wherever it was needed.
        let read = Clipboard::from_foreign(None, Some("file:///home/u/one.txt\r\n")).unwrap();
        assert_eq!(read.mode(), ClipboardMode::Copy);
        // An unknown verb in the GNOME target is not a licence to guess either: it falls through
        // to the uri-list, which is a copy.
        let read = Clipboard::from_foreign(
            Some("obliterate\nfile:///home/u/one.txt"),
            Some("file:///home/u/one.txt\r\n"),
        )
        .unwrap();
        assert_eq!(read.mode(), ClipboardMode::Copy);
    }

    #[test]
    fn clipboard_paste_is_one_request_for_the_whole_set() {
        let destination = PathBuf::from("/home/u/Documents");
        let request = Clipboard::cut(paths())
            .unwrap()
            .paste_into(&destination)
            .unwrap();
        assert_eq!(request.sources, paths(), "one operation naming both files");
        assert_eq!(request.destination, destination);
        assert!(!request.copy, "a cut pastes as a move");

        let request = Clipboard::copy(paths())
            .unwrap()
            .paste_into(&destination)
            .unwrap();
        assert!(request.copy);
    }

    #[test]
    fn clipboard_paste_into_a_source_drops_it_rather_than_asking_for_a_refusal() {
        let clipboard = Clipboard::copy(vec![
            PathBuf::from("/home/u/Documents"),
            PathBuf::from("/home/u/one.txt"),
        ])
        .unwrap();
        let request = clipboard
            .paste_into(Path::new("/home/u/Documents"))
            .unwrap();
        assert_eq!(request.sources, vec![PathBuf::from("/home/u/one.txt")]);
        // And a paste that would do nothing at all is not sent.
        let only_itself = Clipboard::copy(vec![PathBuf::from("/home/u/Documents")]).unwrap();
        assert_eq!(only_itself.paste_into(Path::new("/home/u/Documents")), None);
    }

    #[test]
    fn clipboard_a_cut_is_spent_by_its_paste_and_a_copy_is_not() {
        // Pasting a cut twice would be offering to move files that have already moved.
        assert!(Clipboard::cut(paths()).unwrap().spent_by_paste());
        assert!(!Clipboard::copy(paths()).unwrap().spent_by_paste());
    }

    #[test]
    fn clipboard_of_an_empty_selection_is_no_clipboard_at_all() {
        assert_eq!(Clipboard::copy(Vec::new()), None);
        assert_eq!(Clipboard::cut(Vec::new()), None);
        assert_eq!(Clipboard::from_foreign(None, Some("")), None);
        assert_eq!(Clipboard::from_foreign(Some("cut"), None), None);
    }
}
