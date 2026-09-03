//! `org.freedesktop.impl.portal.FileChooser`.
//!
//! Two halves. The first is agreeing with the rest of the system on "the last folder": the
//! starting directory comes from `Files.Recents1.SuggestDirectory` (unless the caller already
//! gave a more specific `current_folder`), and a result is written back through
//! `Files.Recents1.RecordDirectory` — so a sandboxed application and the file manager can never
//! disagree about where the user was last. The second is turning the request's options into
//! something a person can actually operate: `filters`/`current_filter` narrow the listing,
//! `current_name`/`current_file`/`files` seed a save, and the browsing itself is
//! [`crate::browser::Browser`] — a model with no display in it, so this path is testable.
//!
//! [`NativeUi`] shows that model with Pixelkit; [`NoUiAvailable`] is the honest default when
//! there is no compositor to show it on: nothing is fabricated and the call is `CANCELLED`.

use crate::{
    browser::{FileFilter, FilterPattern},
    clients::RecentsClient,
    request::{owned_value, response, with_request},
};
use async_trait::async_trait;
use lege_filed::{OperationType, Suggestion};
use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// What kind of dialog was requested, and the options specific to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChooserKind {
    Open { multiple: bool, directory: bool },
    Save { current_name: Option<String> },
    SaveFiles { file_names: Vec<String> },
}

/// Everything a [`FileChooserUi`] needs to actually show something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChooserRequest {
    pub kind: ChooserKind,
    pub app_id: String,
    pub title: String,
    pub starting_directory: PathBuf,
    /// The caller's `filters`, in the order offered.
    pub filters: Vec<FileFilter>,
    /// Which of `filters` starts selected; `None` shows everything.
    pub current_filter: Option<usize>,
}

/// What the user picked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChosenFiles {
    pub uris: Vec<String>,
    pub writable: bool,
}

/// The interactive part no toolkit exists to provide yet.
#[async_trait]
pub trait FileChooserUi: Send + Sync + std::fmt::Debug {
    async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles>;
}

/// Headless default: there is nothing to show, so nothing is chosen.
#[derive(Debug, Default)]
pub struct NoUiAvailable;

#[async_trait]
impl FileChooserUi for NoUiAvailable {
    async fn choose(&self, _request: ChooserRequest) -> Option<ChosenFiles> {
        None
    }
}

/// Pixelkit graphical chooser used by the live portal backend.
#[derive(Debug, Default)]
pub struct NativeUi;

#[async_trait]
impl FileChooserUi for NativeUi {
    async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles> {
        tokio::task::spawn_blocking(move || crate::dialog::files(request))
            .await
            .ok()
            .flatten()
    }
}

/// `OperationType` the shared last-directory policy should file this dialog under.
fn operation_type(kind: &ChooserKind) -> OperationType {
    match kind {
        ChooserKind::Open { .. } => OperationType::Open,
        ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. } => OperationType::Save,
    }
}

/// The directory a suggestion resolves to, decoded from `Files.Recents1.SuggestDirectory`'s JSON.
fn suggested_directory(suggestion_json: &str) -> Option<PathBuf> {
    serde_json::from_str::<Suggestion>(suggestion_json)
        .ok()
        .map(|suggestion| suggestion.directory().clone())
}

/// Decide the starting directory: the caller's own `current_folder`, if given, beats our
/// suggestion (they know their own context better than we do); otherwise the shared policy's
/// answer, falling back to `home` if even that is unavailable. Takes `home` as a parameter
/// (rather than reading `$HOME` itself) so it stays a pure function: this crate's tests must run
/// with no process-global state, and `cargo test` runs a binary's tests concurrently, so a
/// helper that mutated `$HOME` for one test could race every other test reading it.
fn resolve_starting_directory(
    caller_current_folder: Option<&Path>,
    suggestion_json: &str,
    home: &Path,
) -> PathBuf {
    caller_current_folder
        .map(Path::to_path_buf)
        .or_else(|| suggested_directory(suggestion_json))
        .unwrap_or_else(|| home.to_path_buf())
}

/// A null-terminated `ay` byte-array path option, as sent by `current_folder`/`current_file`.
fn path_from_bytes(mut bytes: Vec<u8>) -> PathBuf {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    PathBuf::from(OsString::from_vec(bytes))
}

fn get_bytes(options: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<u8>> {
    Vec::<u8>::try_from(options.get(key)?.clone()).ok()
}

fn get_bool(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

fn get_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
}

/// One filter on the wire: `(name, [(type, pattern)])`, where type `0` is a shell glob and `1`
/// is a MIME type (`org.freedesktop.impl.portal.FileChooser`'s `a(sa(us))`).
type WireFilter = (String, Vec<(u32, String)>);

fn filter_from_wire((name, patterns): WireFilter) -> FileFilter {
    FileFilter {
        name,
        patterns: patterns
            .into_iter()
            .map(|(kind, pattern)| {
                if kind == 1 {
                    FilterPattern::Mime(pattern)
                } else {
                    FilterPattern::Glob(pattern)
                }
            })
            .collect(),
    }
}

/// The caller's `filters`, and which one starts selected.
///
/// `current_filter` arrives as a whole filter rather than an index, so it is matched by name
/// against `filters` — and, when the caller sent one that is not in the list (which the spec
/// allows), it is appended, because a filter the application asked for must be offered rather
/// than silently dropped.
fn filters_from_options(options: &HashMap<String, OwnedValue>) -> (Vec<FileFilter>, Option<usize>) {
    let mut filters: Vec<FileFilter> = options
        .get("filters")
        .and_then(|value| Vec::<WireFilter>::try_from(value.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(filter_from_wire)
        .collect();

    let current = options
        .get("current_filter")
        .and_then(|value| WireFilter::try_from(value.clone()).ok())
        .map(filter_from_wire);

    let Some(current) = current else {
        return (filters, None);
    };
    let at = filters
        .iter()
        .position(|filter| filter.name == current.name);
    let index = at.unwrap_or_else(|| {
        filters.push(current);
        filters.len() - 1
    });
    (filters, Some(index))
}

/// A list of null-terminated `ay` paths, as `SaveFiles` sends in `files`.
fn file_names_from_options(options: &HashMap<String, OwnedValue>) -> Vec<String> {
    options
        .get("files")
        .and_then(|value| Vec::<Vec<u8>>::try_from(value.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(path_from_bytes)
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect()
}

/// The reply this backend sends: `(response, results)`.
fn reply(response_code: u32, chosen: Option<&ChosenFiles>) -> (u32, HashMap<String, OwnedValue>) {
    let mut results = HashMap::new();
    if let Some(chosen) = chosen {
        results.insert("uris".to_owned(), owned_value(chosen.uris.clone()));
        results.insert("writable".to_owned(), owned_value(chosen.writable));
    }
    (response_code, results)
}

/// `org.freedesktop.impl.portal.FileChooser`.
#[derive(Debug)]
pub struct FileChooserApi {
    pub recents: Arc<dyn RecentsClient>,
    pub ui: Arc<dyn FileChooserUi>,
}

impl FileChooserApi {
    /// Shared body for `OpenFile`/`SaveFile`/`SaveFiles`: resolve the starting directory, ask the
    /// UI seam, and — only on a real answer — record where the user ended up.
    async fn run(
        &self,
        kind: ChooserKind,
        app_id: &str,
        title: &str,
        options: &HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let operation = operation_type(&kind);
        let suggestion_json = self
            .recents
            .suggest_directory(app_id, operation.as_str())
            .await
            .unwrap_or_default();
        // `current_file` is a whole path the caller proposes to overwrite: its directory is as
        // specific an answer as `current_folder`, so it counts as one.
        let caller_folder = get_bytes(options, "current_folder")
            .map(path_from_bytes)
            .or_else(|| {
                get_bytes(options, "current_file")
                    .map(path_from_bytes)
                    .and_then(|file| file.parent().map(Path::to_path_buf))
            });
        let home = std::env::var("HOME").map_or_else(|_| PathBuf::from("/"), PathBuf::from);
        let starting_directory =
            resolve_starting_directory(caller_folder.as_deref(), &suggestion_json, &home);
        let (filters, current_filter) = filters_from_options(options);

        let request = ChooserRequest {
            kind,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            starting_directory: starting_directory.clone(),
            filters,
            current_filter,
        };

        match self.ui.choose(request).await {
            Some(chosen) => {
                if let Some(first) = chosen.uris.first()
                    && let Some(path) = lege_filed::actions::path_from_uri(first)
                    && let Some(directory) = path.parent()
                {
                    let _ = self
                        .recents
                        .record_directory(app_id, operation.as_str(), &directory.to_string_lossy())
                        .await;
                }
                reply(response::SUCCESS, Some(&chosen))
            }
            None => reply(response::CANCELLED, None),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserApi {
    /// `parent_window` is part of the fixed spec signature on every method here; this backend
    /// needs no window-parenting (no toolkit dialog exists yet — see `FileChooserUi`).
    #[zbus(name = "OpenFile")]
    #[allow(unused_variables)]
    async fn open_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::Open {
            multiple: get_bool(&options, "multiple"),
            directory: get_bool(&options, "directory"),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }

    #[zbus(name = "SaveFile")]
    #[allow(unused_variables)]
    async fn save_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::Save {
            current_name: get_string(&options, "current_name").or_else(|| {
                get_bytes(&options, "current_file")
                    .map(path_from_bytes)
                    .and_then(|file| {
                        file.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
            }),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }

    #[zbus(name = "SaveFiles")]
    #[allow(unused_variables)]
    async fn save_files(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::SaveFiles {
            file_names: file_names_from_options(&options),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callers_own_current_folder_beats_our_suggestion() {
        let suggestion = serde_json::to_string(&Suggestion::Fallback {
            directory: PathBuf::from("/home/u"),
        })
        .unwrap();
        let resolved = resolve_starting_directory(
            Some(Path::new("/home/u/specific")),
            &suggestion,
            Path::new("/home/u"),
        );
        assert_eq!(resolved, PathBuf::from("/home/u/specific"));
    }

    #[test]
    fn falls_back_to_the_shared_suggestion_when_the_caller_gives_none() {
        let suggestion = serde_json::to_string(&Suggestion::CurrentProject {
            project_id: "thesis".into(),
            directory: PathBuf::from("/home/u/thesis"),
        })
        .unwrap();
        assert_eq!(
            resolve_starting_directory(None, &suggestion, Path::new("/home/u")),
            PathBuf::from("/home/u/thesis")
        );
    }

    #[test]
    fn malformed_suggestion_json_falls_back_to_home() {
        assert_eq!(
            resolve_starting_directory(None, "not json", Path::new("/home/fallback")),
            PathBuf::from("/home/fallback")
        );
    }

    #[test]
    fn null_terminated_byte_paths_decode_correctly() {
        let mut bytes = b"/home/u/Downloads".to_vec();
        bytes.push(0);
        assert_eq!(path_from_bytes(bytes), PathBuf::from("/home/u/Downloads"));
        assert_eq!(
            path_from_bytes(b"/no/nul".to_vec()),
            PathBuf::from("/no/nul")
        );
    }

    #[derive(Debug, Default)]
    struct FakeRecents {
        recorded: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl RecentsClient for FakeRecents {
        async fn suggest_directory(
            &self,
            _app_id: &str,
            _operation: &str,
        ) -> anyhow::Result<String> {
            Ok(serde_json::to_string(&Suggestion::Fallback {
                directory: PathBuf::from("/home/u"),
            })
            .unwrap())
        }
        async fn record_directory(
            &self,
            app_id: &str,
            operation: &str,
            directory: &str,
        ) -> anyhow::Result<()> {
            self.recorded.lock().unwrap().push((
                app_id.to_owned(),
                operation.to_owned(),
                directory.to_owned(),
            ));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FixedChoice(Option<ChosenFiles>);

    #[async_trait]
    impl FileChooserUi for FixedChoice {
        async fn choose(&self, _request: ChooserRequest) -> Option<ChosenFiles> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn no_ui_available_answers_cancelled() {
        let api = FileChooserApi {
            recents: Arc::new(FakeRecents::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .run(
                ChooserKind::Open {
                    multiple: false,
                    directory: false,
                },
                "org.legeos.Test",
                "Open",
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::CANCELLED);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn a_chosen_file_is_reported_and_its_directory_recorded() {
        let recents = Arc::new(FakeRecents::default());
        let api = FileChooserApi {
            recents: recents.clone(),
            ui: Arc::new(FixedChoice(Some(ChosenFiles {
                uris: vec!["file:///home/u/report.pdf".to_owned()],
                writable: true,
            }))),
        };
        let (code, results) = api
            .run(
                ChooserKind::Open {
                    multiple: false,
                    directory: false,
                },
                "org.legeos.Test",
                "Open",
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert!(results.contains_key("uris"));
        let recorded = recents.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "org.legeos.Test");
        assert_eq!(recorded[0].2, "/home/u");
    }

    /// The options a browser actually sends when it asks for an upload: named filters, the one
    /// it wants selected, and (for a multi-file save) the names it intends to write. Parsed here
    /// and applied by the browsing model, with no display in the loop.
    #[test]
    fn filechooser_filters_apply() {
        use crate::browser::Browser;
        use lege_filed::{BrowseEntry, EntryKind};

        let images: WireFilter = (
            "Images".to_owned(),
            vec![(0, "*.png".to_owned()), (0, "*.jpg".to_owned())],
        );
        let any_image: WireFilter = ("Any image".to_owned(), vec![(1, "image/*".to_owned())]);
        let mut options = HashMap::new();
        options.insert(
            "filters".to_owned(),
            owned_value(vec![images.clone(), any_image]),
        );
        options.insert("current_filter".to_owned(), owned_value(images));

        let (filters, current) = filters_from_options(&options);
        assert_eq!(filters.len(), 2);
        assert_eq!(current, Some(0), "the caller's current filter, by name");

        let entry = |name: &str, kind| BrowseEntry {
            path: PathBuf::from("/home/u").join(name),
            name: name.to_owned(),
            kind,
            is_symlink: false,
            hidden: false,
            size_bytes: 0,
            mtime_ms: 0,
        };
        let mut browser = Browser::new(
            ChooserKind::Open {
                multiple: false,
                directory: false,
            },
            PathBuf::from("/home/u"),
            filters,
            current,
        );
        browser.set_listing(
            vec![
                entry("Pictures", EntryKind::Directory),
                entry("photo.PNG", EntryKind::File),
                entry("scan.jpg", EntryKind::File),
                entry("notes.txt", EntryKind::File),
            ],
            false,
        );
        let visible: Vec<&str> = browser
            .visible()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            visible,
            ["Pictures", "photo.PNG", "scan.jpg"],
            "globs match whatever the case, and a filter never hides a folder"
        );

        // A MIME filter admits everything: this process has no content detector, and guessing
        // would hide the very file the person came to upload.
        browser.set_current_filter(Some(1));
        assert_eq!(browser.visible().len(), 4);
        browser.set_current_filter(None);
        assert_eq!(browser.visible().len(), 4);

        // A filter the caller marked current but did not list is offered rather than dropped.
        let mut only_current = HashMap::new();
        only_current.insert(
            "current_filter".to_owned(),
            owned_value(("Archives".to_owned(), vec![(0_u32, "*.tar.gz".to_owned())])),
        );
        let (filters, current) = filters_from_options(&only_current);
        assert_eq!(filters.len(), 1);
        assert_eq!(current, Some(0));
        assert!(filters[0].accepts("backup.tar.gz"));
        assert!(!filters[0].accepts("backup.zip"));

        // `SaveFiles` sends whole null-terminated paths; only their names matter.
        let mut saving = HashMap::new();
        let mut first = b"/tmp/one.txt".to_vec();
        first.push(0);
        saving.insert(
            "files".to_owned(),
            owned_value(vec![first, b"two.txt".to_vec()]),
        );
        assert_eq!(file_names_from_options(&saving), ["one.txt", "two.txt"]);
    }
}
